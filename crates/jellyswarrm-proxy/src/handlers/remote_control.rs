//! Delivering Jellyfin's remote-control play commands to clients connected to the proxy.
//!
//! `POST /Sessions/{id}/Playing` is how one client tells another to start playing — "cast this
//! there". Jellyfin implements it by pushing a `Play` message down the *target* client's websocket.
//!
//! That breaks through the proxy for a structural reason: the proxy terminates the client's
//! websocket itself (it implements SyncPlay locally) rather than relaying the upstream's. So the
//! request reaches the upstream, the upstream accepts it and pushes `Play` to its own sockets — and
//! the client, whose socket is here, never hears it. The request succeeds and nothing happens,
//! which is exactly what a plugin like Media Bar reports as a play button that does nothing.
//!
//! The proxy does not need to relay anything to fix this. It already holds the client's socket and
//! already pushes messages down it, so it can deliver the command directly.
//!
//! Two details are load-bearing:
//!
//! * **The ids must stay virtual.** jellyfin-web takes `serverId` from its own connection — the
//!   proxy — and hands `msg.Data.ItemIds` straight to the playback manager, which resolves them
//!   against that server. Sending upstream ids here would play the wrong item, or nothing, rather
//!   than failing visibly.
//! * **Unidentified targets fall through to the upstream.** Casting to a device that is not
//!   connected to this proxy must keep working as before, and guessing would play media on someone
//!   else's screen.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::{extractors::Preprocessed, AppState};

/// The `Play` message payload jellyfin-web expects.
///
/// Field names and shape are taken from jellyfin-web's `serverNotifications.js`, which reads
/// `msg.Data.ItemIds`, `PlayCommand`, `StartPositionTicks`, `MediaSourceId`, `AudioStreamIndex`,
/// `SubtitleStreamIndex` and `StartIndex`.
#[derive(Debug, Serialize)]
pub struct PlayMessage {
    #[serde(rename = "ItemIds")]
    pub item_ids: Vec<String>,
    #[serde(rename = "PlayCommand")]
    pub play_command: String,
    #[serde(rename = "StartPositionTicks", skip_serializing_if = "Option::is_none")]
    pub start_position_ticks: Option<i64>,
    #[serde(rename = "MediaSourceId", skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,
    #[serde(rename = "AudioStreamIndex", skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,
    #[serde(
        rename = "SubtitleStreamIndex",
        skip_serializing_if = "Option::is_none"
    )]
    pub subtitle_stream_index: Option<i32>,
    #[serde(rename = "StartIndex", skip_serializing_if = "Option::is_none")]
    pub start_index: Option<i32>,
}

/// Query parameters Jellyfin accepts on `POST /Sessions/{id}/Playing`.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct PlayQuery {
    #[serde(alias = "itemIds", alias = "ItemIds")]
    pub item_ids: Option<String>,
    #[serde(alias = "playCommand", alias = "PlayCommand")]
    pub play_command: Option<String>,
    #[serde(alias = "startPositionTicks", alias = "StartPositionTicks")]
    pub start_position_ticks: Option<i64>,
    #[serde(alias = "mediaSourceId", alias = "MediaSourceId")]
    pub media_source_id: Option<String>,
    #[serde(alias = "audioStreamIndex", alias = "AudioStreamIndex")]
    pub audio_stream_index: Option<i32>,
    #[serde(alias = "subtitleStreamIndex", alias = "SubtitleStreamIndex")]
    pub subtitle_stream_index: Option<i32>,
    #[serde(alias = "startIndex", alias = "StartIndex")]
    pub start_index: Option<i32>,
}

/// Splits a comma-separated id list, discarding empties.
pub fn split_ids(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether a websocket session key belongs to this user and device.
///
/// Keys are `{user_id}:{device_id}:{token_fingerprint}`, so the first two segments identify the
/// client without needing its token. Matching on both is what keeps a command aimed at one device
/// from being delivered to another belonging to the same user.
pub fn session_key_matches(key: &str, user_id: &str, device_id: &str) -> bool {
    let mut parts = key.splitn(3, ':');
    let (Some(key_user), Some(key_device)) = (parts.next(), parts.next()) else {
        return false;
    };
    key_user == user_id && !device_id.is_empty() && key_device == device_id
}

/// `POST /Sessions/{sessionid}/Playing`
///
/// The path id is already de-virtualized by the url processor, so `preprocessed.request` carries
/// the upstream session id while `preprocessed.original_request` still carries the virtual ids the
/// client sent — which is what the client needs back.
pub async fn post_sessions_playing(
    Path(_sessionid): Path<String>,
    Query(query): Query<PlayQuery>,
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Response {
    // Ids as the CLIENT sent them: virtual. jellyfin-web resolves them against its own connection,
    // which is this proxy, so these must not be the upstream's ids.
    let virtual_item_ids: Vec<String> = preprocessed
        .original_request
        .url()
        .query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case("itemIds"))
        .map(|(_, value)| split_ids(&value))
        .unwrap_or_default();

    let Some(user) = preprocessed.user.as_ref() else {
        return forward_upstream(state, preprocessed).await;
    };

    if virtual_item_ids.is_empty() {
        debug!("Remote play command carried no item ids; forwarding upstream");
        return forward_upstream(state, preprocessed).await;
    }

    // Identify the target device from the upstream session list. Falling through when it cannot be
    // identified is deliberate: delivering to the wrong socket would start playback on someone
    // else's screen, which is worse than the command not arriving.
    let Some(device_id) = target_device_id(&state, &preprocessed).await else {
        debug!("Could not identify the target device for a remote play; forwarding upstream");
        return forward_upstream(state, preprocessed).await;
    };

    let keys = state.syncplay.connected_session_keys().await;
    let Some(session_key) = keys
        .into_iter()
        .find(|key| session_key_matches(key, &user.id, &device_id))
    else {
        debug!("Remote play target {device_id} is not connected here; forwarding upstream");
        return forward_upstream(state, preprocessed).await;
    };

    let message = PlayMessage {
        item_ids: virtual_item_ids,
        play_command: query.play_command.unwrap_or_else(|| "PlayNow".to_string()),
        start_position_ticks: query.start_position_ticks,
        media_source_id: query.media_source_id,
        audio_stream_index: query.audio_stream_index,
        subtitle_stream_index: query.subtitle_stream_index,
        start_index: query.start_index,
    };

    if state
        .syncplay
        .try_send_message(&session_key, "Play", &message)
        .await
    {
        debug!("Delivered a Play command directly to {session_key}");
        return StatusCode::NO_CONTENT.into_response();
    }

    warn!(
        "Target session vanished before the Play command could be delivered; forwarding upstream"
    );
    forward_upstream(state, preprocessed).await
}

/// Reads the target session's device id from the upstream session list.
async fn target_device_id(
    state: &AppState,
    preprocessed: &crate::request_preprocessing::PreprocessedRequest,
) -> Option<String> {
    // The path id has already been rewritten to the upstream's own session id.
    let upstream_session_id = preprocessed
        .request
        .url()
        .path_segments()?
        .nth(1)?
        .to_string();

    let session = preprocessed.session.as_ref()?;
    let mut url = url::Url::parse(preprocessed.server.url.as_str()).ok()?;
    url.set_path("/Sessions");

    let response = state
        .reqwest_client
        .get(url)
        .header("X-Emby-Token", &session.jellyfin_token)
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let sessions: Vec<serde_json::Value> = response.json().await.ok()?;
    sessions.iter().find_map(|entry| {
        let id = entry.get("Id")?.as_str()?;
        (id.eq_ignore_ascii_case(&upstream_session_id))
            .then(|| entry.get("DeviceId")?.as_str().map(str::to_string))
            .flatten()
    })
}

/// Relays the request to the upstream unchanged, which is the pre-existing behaviour.
async fn forward_upstream(
    state: AppState,
    preprocessed: crate::request_preprocessing::PreprocessedRequest,
) -> Response {
    match crate::handlers::common::execute_json_request::<serde_json::Value>(
        &state.reqwest_client,
        preprocessed.request,
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        // Jellyfin answers this endpoint with 204 and an empty body, which is not valid JSON, so a
        // decode failure here means the upstream accepted it rather than that anything went wrong.
        Err(StatusCode::INTERNAL_SERVER_ERROR) => StatusCode::NO_CONTENT.into_response(),
        Err(status) => status.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_split_on_commas_and_ignore_blanks() {
        assert_eq!(split_ids("a,b,c"), vec!["a", "b", "c"]);
        assert_eq!(split_ids(" a , b "), vec!["a", "b"]);
        assert_eq!(split_ids("a,,b"), vec!["a", "b"]);
        assert!(split_ids("").is_empty());
        assert!(split_ids(" , ").is_empty());
    }

    /// The key is `{user}:{device}:{token-fingerprint}`. Matching on user alone would deliver a
    /// command aimed at one of a user's devices to whichever of their devices connected first.
    #[test]
    fn a_session_key_matches_only_its_own_user_and_device() {
        let key = "user-1:device-abc:fingerprint";

        assert!(session_key_matches(key, "user-1", "device-abc"));
        assert!(!session_key_matches(key, "user-2", "device-abc"));
        assert!(
            !session_key_matches(key, "user-1", "device-xyz"),
            "another device belonging to the same user must not match"
        );
    }

    /// Token-only sessions have no device, and must never match — otherwise a command with no
    /// identifiable target would be delivered to an arbitrary socket.
    #[test]
    fn a_session_without_a_device_never_matches() {
        assert!(!session_key_matches(
            "user-1:token:fingerprint",
            "user-1",
            ""
        ));
        assert!(!session_key_matches("user-1", "user-1", "device-abc"));
        assert!(!session_key_matches("", "user-1", "device-abc"));
    }

    /// The message must match what jellyfin-web reads in `serverNotifications.js`: `ItemIds`,
    /// `PlayCommand`, and optional playback positioning fields. Absent optionals are omitted rather
    /// than sent as null, matching Jellyfin's own serialization.
    #[test]
    fn the_play_message_matches_what_the_client_reads() {
        let message = PlayMessage {
            item_ids: vec!["abc".to_string(), "def".to_string()],
            play_command: "PlayNow".to_string(),
            start_position_ticks: None,
            media_source_id: None,
            audio_stream_index: None,
            subtitle_stream_index: None,
            start_index: None,
        };

        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["ItemIds"], serde_json::json!(["abc", "def"]));
        assert_eq!(json["PlayCommand"], "PlayNow");
        assert!(
            json.get("StartPositionTicks").is_none(),
            "absent optionals must be omitted, not null"
        );
    }

    /// PlayNext and PlayLast queue rather than play in jellyfin-web, so the command has to survive
    /// verbatim instead of being normalised to PlayNow.
    #[test]
    fn queue_commands_are_preserved() {
        for command in ["PlayNow", "PlayNext", "PlayLast"] {
            let message = PlayMessage {
                item_ids: vec!["abc".to_string()],
                play_command: command.to_string(),
                start_position_ticks: None,
                media_source_id: None,
                audio_stream_index: None,
                subtitle_stream_index: None,
                start_index: None,
            };
            let json = serde_json::to_value(&message).unwrap();
            assert_eq!(json["PlayCommand"], command);
        }
    }
}
