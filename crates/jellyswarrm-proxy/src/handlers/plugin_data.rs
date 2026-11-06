//! Merging a plugin's per-item data across the swarm.
//!
//! [`crate::request_preprocessing::resolve_server`] pins a plugin's API calls to the server that
//! injected its script, because those routes carry no media ids to resolve from and would otherwise
//! be answered by a server that may not run the plugin. That is the right answer for a route
//! describing the plugin or the user, and the wrong one for a route describing the library: a
//! pinned server only knows its own items, so in a merged library every item from anywhere else
//! goes untagged.
//!
//! These routes are asked of every server the user has a session on instead. Each answer is
//! processed against the server that gave it, which puts its ids into the proxy's id space, and the
//! results are merged — by then the ids no longer collide, because they no longer belong to any one
//! server.

use std::collections::HashSet;

use axum::http::StatusCode;
use serde_json::{map::Entry, Value};
use tokio::task::JoinSet;
use tracing::{debug, error, warn};

use crate::{
    processors::response_processor::ResponseProcessingProfile,
    request_preprocessing::{
        apply_to_request, remap_authorization, JellyfinAuthorization, PreprocessedRequest,
    },
    server_storage::Server,
    user_authorization_service::AuthorizationSession,
    AppState,
};

/// Whether `path` names a plugin route whose answer is per-item and so belongs to every server.
pub async fn is_federated_plugin_path(state: &AppState, path: &str) -> bool {
    let config = state.config.read().await;
    config
        .plugin_federation
        .federated_plugin_api_prefixes
        .iter()
        .any(|prefix| path_matches_prefix(path, prefix))
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return false;
    }

    // A prefix must end at a segment boundary so `/JellyfinEnhanced/tag-cache` cannot also claim a
    // hypothetical `/JellyfinEnhanced/tag-cache-admin`.
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/') || rest.starts_with('?'))
}

/// Ask every server the user has a session on, and merge what comes back.
///
/// Returns `None` when the request is not one to federate — an unmatched path, an unauthenticated
/// caller, or a user with a session on a single server, where the pinned path already does the
/// right thing for one fewer round trip.
pub async fn merged_plugin_data(
    state: &AppState,
    preprocessed: &PreprocessedRequest,
) -> Option<Result<Value, StatusCode>> {
    let path = preprocessed.original_request.url().path().to_string();
    if !is_federated_plugin_path(state, &path).await {
        return None;
    }

    let sessions = one_session_per_server(preprocessed.sessions.as_ref()?);
    if sessions.len() < 2 {
        return None;
    }

    debug!(
        "Federating plugin data request {} across {} servers",
        path,
        sessions.len()
    );
    fan_out(state, preprocessed, &sessions).await.map(Ok)
}

/// One leg per server, rather than one per session.
///
/// A user accumulates a session per device per server, so this list routinely holds several entries
/// for the same server — seven, for someone using a browser and a media player against two servers.
/// Fanning out per session asks each server the same question several times, and these payloads run
/// to five figures: the duplicate answers cost a multiple of the work to fetch, parse and merge for
/// a result identical to asking once. Sessions arrive in server priority order, so keeping the first
/// of each preserves the ordering the merge depends on to resolve scalar conflicts.
fn one_session_per_server(
    sessions: &[(AuthorizationSession, Server)],
) -> Vec<(AuthorizationSession, Server)> {
    let mut seen = HashSet::new();
    sessions
        .iter()
        .filter(|(_, server)| seen.insert(server.id.as_i64()))
        .cloned()
        .collect()
}

/// Returns `None` when no server had anything to say, leaving the caller to take the ordinary
/// pinned path.
///
/// That case is not an error and must not be reported as one. A plugin answers "I hold nothing for
/// you" with its own status — Jellyfin Enhanced 404s a tag cache it has not built yet — and the
/// client is written to handle exactly that, falling back to computing what it needs itself.
/// Synthesising a gateway error instead breaks a client that would otherwise have coped, and takes
/// the plugin's unrelated features down with it. Falling through returns whatever the upstream
/// actually said, which is what the client saw before any of this federated.
async fn fan_out(
    state: &AppState,
    preprocessed: &PreprocessedRequest,
    sessions: &[(AuthorizationSession, Server)],
) -> Option<Value> {
    let mut legs = JoinSet::new();
    for (index, (session, server)) in sessions.iter().enumerate() {
        let Some(request) = preprocessed.original_request.try_clone() else {
            // Only a streaming body defeats `try_clone`, and a plugin data route does not have one.
            error!("Cannot federate a plugin data request whose body cannot be replayed");
            return None;
        };

        let state = state.clone();
        let session = session.clone();
        let server = server.clone();
        let auth = preprocessed.auth.clone();
        let access_scope = preprocessed.access_scope.clone();
        legs.spawn(async move {
            let result =
                fetch_from_server(&state, request, auth, session, &server, access_scope).await;
            (index, server, result)
        });
    }

    let mut answers: Vec<(usize, Value)> = Vec::new();
    while let Some(leg) = legs.join_next().await {
        match leg {
            Ok((index, _, Ok(value))) => answers.push((index, value)),
            Ok((_, server, Err(e))) => {
                warn!(
                    "Plugin data leg to '{}' failed: {:?}; its items will carry no plugin data",
                    server.name, e
                );
            }
            Err(e) => error!("Plugin data leg panicked: {e}"),
        }
    }

    let merged = merge_answers(answers);
    if merged.is_none() {
        debug!("No server had plugin data to merge; falling back to the pinned response");
    }
    merged
}

/// Combine the legs that answered, or `None` if none did.
///
/// Legs are merged in server priority order rather than in the order they finished, because that is
/// what decides which value survives a scalar conflict — otherwise a payload's own version marker
/// would depend on which server happened to be quicker.
fn merge_answers(mut answers: Vec<(usize, Value)>) -> Option<Value> {
    answers.sort_by_key(|(index, _)| *index);

    let mut merged: Option<Value> = None;
    for (_, answer) in answers {
        match &mut merged {
            None => merged = Some(answer),
            Some(merged) => merge_into(merged, answer),
        }
    }

    merged
}

async fn fetch_from_server(
    state: &AppState,
    mut request: reqwest::Request,
    auth: Option<JellyfinAuthorization>,
    session: AuthorizationSession,
    server: &Server,
    access_scope: Option<crate::virtual_library_service::VirtualLibraryAccessScope>,
) -> Result<Value, StatusCode> {
    let session = Some(session);
    let new_auth = remap_authorization(&auth, &session).await.map_err(|e| {
        error!("Failed to remap authorization for '{}': {e}", server.name);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Re-targets the URL against this server, which is also what substitutes this server's own user
    // id for the proxy's in the path — every leg addresses a different one.
    apply_to_request(
        &mut request,
        server,
        &session,
        &new_auth,
        state,
        access_scope.as_ref(),
    )
    .await;

    let url = request.url().clone();
    let response = state.reqwest_client.execute(request).await.map_err(|e| {
        error!("Plugin data request to {url} failed: {e}");
        StatusCode::BAD_GATEWAY
    })?;

    if !response.status().is_success() {
        // A plugin says "nothing here for this user" with a 404 — a cache it has not built yet —
        // and that is an ordinary answer from a healthy server, not a fault worth a warning.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            debug!("{} holds no plugin data at {url}", server.name);
        } else {
            warn!(
                "Plugin data request to {url} returned {}",
                response.status()
            );
        }
        return Err(StatusCode::BAD_GATEWAY);
    }

    let mut value: Value = response.json().await.map_err(|e| {
        error!("Plugin data response from {url} was not JSON: {e}");
        StatusCode::BAD_GATEWAY
    })?;

    let proxy_api_key = auth
        .as_ref()
        .and_then(|auth: &JellyfinAuthorization| auth.token_ref());
    state
        .process_response_json(
            &mut value,
            server,
            ResponseProcessingProfile::BestEffortMedia,
            false,
            proxy_api_key,
        )
        .await?;

    Ok(value)
}

/// Merge `incoming` into `target`, preferring what `target` already holds.
///
/// Objects union, which is the whole point: a plugin keyed by item id contributes one server's
/// items per leg and, once processed, no two legs claim the same key. Arrays concatenate. Scalars
/// keep the higher-priority server's value, so a payload's own `version` or format marker stays a
/// single coherent value rather than the last one to arrive.
fn merge_into(target: &mut Value, incoming: Value) {
    match (target, incoming) {
        (Value::Object(target), Value::Object(incoming)) => {
            for (key, value) in incoming {
                match target.entry(key) {
                    Entry::Vacant(slot) => {
                        slot.insert(value);
                    }
                    Entry::Occupied(mut slot) => merge_into(slot.get_mut(), value),
                }
            }
        }
        (Value::Array(target), Value::Array(incoming)) => {
            target.extend(incoming);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_prefix_must_end_at_a_segment_boundary() {
        assert!(path_matches_prefix(
            "/JellyfinEnhanced/tag-cache/abc",
            "/JellyfinEnhanced/tag-cache"
        ));
        assert!(path_matches_prefix(
            "/JellyfinEnhanced/tag-cache",
            "/JellyfinEnhanced/tag-cache"
        ));
        assert!(
            !path_matches_prefix(
                "/JellyfinEnhanced/tag-cache-admin",
                "/JellyfinEnhanced/tag-cache"
            ),
            "a longer sibling route must not be claimed by a shorter prefix"
        );
        assert!(!path_matches_prefix(
            "/JellyfinEnhanced/version",
            "/JellyfinEnhanced/tag-cache"
        ));
    }

    /// Federation is a subset of pinning, so every federated prefix has to be one the pinning list
    /// already claims - otherwise the route is federated but its legs are never authenticated
    /// against the right servers.
    #[test]
    fn every_federated_prefix_is_also_a_pinned_one() {
        let config = crate::config::PluginFederationConfig::default();

        for federated in &config.federated_plugin_api_prefixes {
            assert!(
                config
                    .plugin_api_prefixes
                    .iter()
                    .any(|pinned| path_matches_prefix(federated, pinned)),
                "{federated} is federated but not pinned"
            );
        }
    }

    /// The case this exists for: each server contributes its own items, keyed by ids the proxy has
    /// already rewritten into its own space, so the union is the whole library's worth of tags.
    #[test]
    fn item_keyed_objects_from_each_server_are_unioned() {
        let mut merged = json!({ "version": 1, "items": { "sgtv-item": { "quality": "1080P" } } });
        merge_into(
            &mut merged,
            json!({ "version": 1, "items": { "zulu-item": { "quality": "4K" } } }),
        );

        let items = merged["items"].as_object().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items["sgtv-item"]["quality"], "1080P");
        assert_eq!(items["zulu-item"]["quality"], "4K");
    }

    /// A format marker must not flip to whichever leg answered last; the highest-priority server
    /// merges first and keeps it.
    #[test]
    fn scalars_keep_the_first_servers_value() {
        let mut merged = json!({ "version": 1, "items": {} });
        merge_into(&mut merged, json!({ "version": 2, "items": {} }));

        assert_eq!(merged["version"], 1);
    }

    #[test]
    fn arrays_concatenate() {
        let mut merged = json!({ "tags": ["a"] });
        merge_into(&mut merged, json!({ "tags": ["b"] }));

        assert_eq!(merged["tags"], json!(["a", "b"]));
    }

    fn session_on(server_id: i64, device: &str) -> (AuthorizationSession, Server) {
        use crate::{config::MediaStreamingMode, server_id::ServerId, server_url::ServerUrl};

        let server = Server {
            id: ServerId::new(server_id),
            name: format!("server-{server_id}"),
            url: ServerUrl::parse(&format!("http://server-{server_id}.example")).unwrap(),
            priority: 100 - server_id as i32,
            media_streaming_mode: MediaStreamingMode::Proxy,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let session = AuthorizationSession {
            id: server_id,
            user_id: "user".into(),
            mapping_id: server_id,
            server_url: server.url.to_string(),
            device: crate::user_authorization_service::Device {
                client: "client".to_string(),
                device: device.to_string(),
                device_id: format!("{device}-id"),
                version: "1".to_string(),
            },
            jellyfin_token: "token".to_string(),
            original_user_id: "upstream".to_string(),
            expires_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        (session, server)
    }

    /// A user collects a session per device per server, and a token-only plugin call matches all of
    /// them. Fanning out per session asked two servers the same question seven times and merged
    /// seven copies of a five-figure payload — identical output for several times the work, slow
    /// enough that JMP gave up while the browser waited.
    #[test]
    fn each_server_is_asked_once_however_many_devices_the_user_has() {
        let sessions = vec![
            session_on(1, "Gaming"),
            session_on(2, "Gaming"),
            session_on(2, "Chrome"),
            session_on(1, "Chrome"),
            session_on(2, "Phone"),
        ];

        let deduped = one_session_per_server(&sessions);

        assert_eq!(deduped.len(), 2);
        assert_eq!(
            deduped
                .iter()
                .map(|(_, s)| s.id.as_i64())
                .collect::<Vec<_>>(),
            vec![1, 2],
            "the first session for each server wins, keeping server priority order"
        );
    }

    /// The regression this guards: an operator whose tag cache has not been built anywhere gets a
    /// 404 from every leg. Reporting that as a gateway error broke Jellyfin Enhanced outright —
    /// it handles a 404 by computing badges itself, and a 502 instead took its Requests and
    /// Calendar pages down with it. Merging nothing must fall through to the pinned response.
    #[test]
    fn nothing_to_merge_is_not_an_error() {
        let answers: Vec<(usize, Value)> = Vec::new();

        assert!(
            merge_answers(answers).is_none(),
            "no answers must fall through rather than synthesise a failure"
        );
    }

    #[test]
    fn answers_merge_in_server_priority_order_not_arrival_order() {
        let merged = merge_answers(vec![
            (1, json!({ "version": 2, "items": { "b": 1 } })),
            (0, json!({ "version": 1, "items": { "a": 1 } })),
        ])
        .unwrap();

        assert_eq!(merged["version"], 1, "the first server's scalar wins");
        assert_eq!(merged["items"].as_object().unwrap().len(), 2);
    }

    /// One server means the pinned path already gives the same answer, so federating would only add
    /// a round trip.
    #[test]
    fn a_single_server_is_not_federated() {
        let sessions = vec![session_on(1, "Gaming"), session_on(1, "Chrome")];

        assert_eq!(one_session_per_server(&sessions).len(), 1);
    }

    /// A plugin that keys by item id at the root, with no container field, merges the same way.
    #[test]
    fn root_keyed_payloads_are_unioned() {
        let mut merged = json!({ "sgtv-item": { "rating": 7.4 } });
        merge_into(&mut merged, json!({ "zulu-item": { "rating": 9.1 } }));

        assert_eq!(merged.as_object().unwrap().len(), 2);
    }
}
