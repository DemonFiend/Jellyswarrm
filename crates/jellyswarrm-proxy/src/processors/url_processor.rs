use anyhow::Result;
use tracing::debug;

use crate::{
    media_storage_service::MediaMapping,
    server_id::ServerId,
    server_storage::Server,
    url_helper::{contains_id, is_id_like, replace_id},
    user_authorization_service::AuthorizationSession,
    virtual_library_service::{VirtualLibraryAccessScope, VirtualLibraryResolution},
    DataContext,
};

pub static MEDIA_ID_PATH_TAGS: &[&str] = &[
    "Items",
    // Session ids are minted by the proxy when it rewrites `/Sessions` responses, so they have to
    // be translated back on the way in. Without this, remote-control endpoints — including
    // `POST /Sessions/{id}/Playing`, which is how Media Bar's play button starts playback — send an
    // id the upstream has never issued and get a 404. Only UUID-shaped segments are matched, so
    // `/Sessions/Playing` and `/Sessions/Capabilities/Full` are unaffected.
    "Sessions",
    "Audio",
    "Shows",
    "Videos",
    "Playlists",
    "Collections",
    "PlayedItems",
    "FavoriteItems",
    "MediaSegments",
    "PlayingItems",
    "Recordings",
    "Channels",
    "Programs",
    "SeriesTimers",
    "Timers",
    "UserFavoriteItems",
    "UserItems",
    "UserPlayedItems",
];

// Image `Tag` query values are opaque cache tokens and must not be remapped.
pub static MEDIA_ID_QUERY_TAGS: &[&str] = &[
    "AlbumId",
    "AlbumIds",
    "ParentId",
    "ItemId",
    "ItemIds",
    "ExcludeItemIds",
    "AncestorIds",
    "SeriesId",
    "MediaSourceId",
    "SeasonId",
    "startItemId",
    "IDs",
    "PersonIds",
    "ArtistIds",
    "ContributingArtistIds",
    "AlbumArtistIds",
    "ExcludeArtistIds",
];

pub static USER_ID_PATH_TAGS: &[&str] = &["Users"];
pub static USER_ID_QUERY_TAGS: &[&str] = &["UserId"];
pub static API_KEY_QUERY_TAGS: &[&str] = &["api_key", "ApiKey"];

pub struct UrlProcessor {
    data_context: DataContext,
}

impl UrlProcessor {
    pub fn new(data_context: DataContext) -> Self {
        Self { data_context }
    }

    pub async fn client_to_server_url(
        &self,
        url: &mut url::Url,
        session: &Option<AuthorizationSession>,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) {
        self.replace_user_ids_in_path(url, session);
        self.replace_media_ids_in_path(url, access_scope, required_server_id)
            .await;

        let mut pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();

        self.replace_session_query_values(&mut pairs, session);
        self.replace_media_ids_in_query(&mut pairs, access_scope, required_server_id)
            .await;

        url.query_pairs_mut().clear().extend_pairs(pairs);
    }

    pub async fn server_to_client_delivery_url(
        &self,
        value: &str,
        server: &Server,
        proxy_api_key: Option<&str>,
    ) -> Result<Option<String>> {
        let Some((mut url, style)) = parse_delivery_url(value) else {
            return Ok(None);
        };

        self.remap_delivery_url_path(&mut url, server).await?;
        self.remap_delivery_url_query(&mut url, server, proxy_api_key)
            .await?;

        Ok(Some(format_delivery_url(url, style)))
    }

    pub async fn server_from_client_url(
        &self,
        url: &url::Url,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<Server>> {
        if let Some(server) = self.server_from_path_media_ids(url, access_scope).await? {
            return Ok(Some(server));
        }

        self.server_from_query_media_ids(url, access_scope).await
    }

    fn replace_user_ids_in_path(&self, url: &mut url::Url, session: &Option<AuthorizationSession>) {
        let Some(session) = session else {
            return;
        };

        for &path_segment in USER_ID_PATH_TAGS {
            if let Some(user_id) = contains_id(url, path_segment) {
                debug!(
                    "Replacing user ID in path: {} -> {}",
                    user_id, session.original_user_id
                );
                *url = replace_id(url.clone(), &user_id, &session.original_user_id);
            }
        }

        replace_proxy_user_id_in_path(url, session);
    }

    async fn replace_media_ids_in_path(
        &self,
        url: &mut url::Url,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) {
        for &path_segment in MEDIA_ID_PATH_TAGS {
            if let Some(media_id) = contains_id(url, path_segment) {
                if let Some(media_mapping) = self
                    .client_media_mapping(&media_id, access_scope, required_server_id)
                    .await
                {
                    debug!(
                        "Replacing media ID in path: {} -> {}",
                        media_id, media_mapping.original_media_id
                    );
                    *url = replace_id(url.clone(), &media_id, &media_mapping.original_media_id);
                }
            }
        }
    }

    fn replace_session_query_values(
        &self,
        pairs: &mut [(String, String)],
        session: &Option<AuthorizationSession>,
    ) {
        let Some(session) = session else {
            return;
        };

        for (name, value) in pairs {
            if matches_case_insensitive(name, USER_ID_QUERY_TAGS) {
                *value = session.original_user_id.clone();
            } else if matches_case_insensitive(name, API_KEY_QUERY_TAGS) {
                *value = session.jellyfin_token.clone();
            }
        }
    }

    async fn replace_media_ids_in_query(
        &self,
        pairs: &mut [(String, String)],
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) {
        for (name, value) in pairs {
            if !matches_case_insensitive(name, MEDIA_ID_QUERY_TAGS) {
                continue;
            }

            if let Some(resolved_value) = self
                .resolve_client_media_id_list(value, access_scope, required_server_id)
                .await
            {
                *value = resolved_value;
            }
        }
    }

    async fn resolve_client_media_id_list(
        &self,
        value: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) -> Option<String> {
        let mut changed = false;
        let mut resolved_ids = Vec::new();

        for raw_id in value.split(',') {
            let trimmed = raw_id.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some(media_mapping) = self
                .client_media_mapping(trimmed, access_scope, required_server_id)
                .await
            {
                debug!(
                    "Replacing media ID in query: {} -> {}",
                    trimmed, media_mapping.original_media_id
                );
                resolved_ids.push(media_mapping.original_media_id);
                changed = true;
            } else {
                resolved_ids.push(trimmed.to_string());
            }
        }

        changed.then(|| resolved_ids.join(","))
    }

    async fn remap_delivery_url_path(&self, url: &mut url::Url, server: &Server) -> Result<()> {
        let Some(segments) = url.path_segments() else {
            return Ok(());
        };

        let mut changed = false;
        let mut remapped_segments = Vec::new();

        for segment in segments {
            if is_id_like(segment) {
                remapped_segments.push(self.virtual_media_id(segment, server).await?);
                changed = true;
            } else {
                remapped_segments.push(segment.to_string());
            }
        }

        if changed {
            url.set_path(&remapped_segments.join("/"));
        }

        Ok(())
    }

    async fn remap_delivery_url_query(
        &self,
        url: &mut url::Url,
        server: &Server,
        proxy_api_key: Option<&str>,
    ) -> Result<()> {
        let Some(query) = url.query() else {
            return Ok(());
        };

        let mut changed = false;
        let mut pairs = Vec::new();

        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if matches_case_insensitive(&key, API_KEY_QUERY_TAGS) {
                changed = true;
                if let Some(proxy_api_key) = proxy_api_key {
                    pairs.push((key.into_owned(), proxy_api_key.to_string()));
                }
                continue;
            }

            let value = if matches_case_insensitive(&key, MEDIA_ID_QUERY_TAGS) {
                let remapped = self.remap_delivery_url_query_value(&value, server).await?;
                if remapped != value {
                    changed = true;
                }
                remapped
            } else {
                value.into_owned()
            };

            pairs.push((key.into_owned(), value));
        }

        if changed {
            url.query_pairs_mut().clear().extend_pairs(pairs);
        }

        Ok(())
    }

    async fn remap_delivery_url_query_value(&self, value: &str, server: &Server) -> Result<String> {
        let mut changed = false;
        let mut remapped_ids = Vec::new();

        for raw_id in value.split(',') {
            let id = raw_id.trim();
            if id.is_empty() {
                continue;
            }

            if is_id_like(id) {
                remapped_ids.push(self.virtual_media_id(id, server).await?);
                changed = true;
            } else {
                remapped_ids.push(id.to_string());
            }
        }

        if changed {
            Ok(remapped_ids.join(","))
        } else {
            Ok(value.to_string())
        }
    }

    async fn virtual_media_id(&self, id: &str, server: &Server) -> Result<String> {
        self.data_context
            .media_storage
            .get_or_create_media_mapping(id, server)
            .await
            .map(|mapping| mapping.virtual_media_id)
            .map_err(Into::into)
    }

    async fn client_media_mapping(
        &self,
        virtual_media_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) -> Option<MediaMapping> {
        if let Some(mapping) = self
            .data_context
            .media_storage
            .get_media_mapping_by_virtual(virtual_media_id)
            .await
            .unwrap_or_default()
        {
            if !server_is_allowed(mapping.server_id, access_scope, required_server_id) {
                return None;
            }
            return Some(mapping);
        }

        self.data_context
            .virtual_library_service
            .routing_target(virtual_media_id, access_scope, required_server_id)
            .await
            .ok()
            .flatten()
            .map(|target| target.mapping)
    }

    async fn server_from_path_media_ids(
        &self,
        url: &url::Url,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<Server>> {
        for &path_segment in MEDIA_ID_PATH_TAGS {
            if let Some(media_id) = contains_id(url, path_segment) {
                debug!("Found {} ID in request: {}", path_segment, media_id);
                if let Some(server) = self
                    .server_from_client_media_id(&media_id, access_scope)
                    .await?
                {
                    debug!(
                        "Found server for {} ID {}: {} ({})",
                        path_segment, media_id, server.name, server.url
                    );
                    return Ok(Some(server));
                }
                debug!("No server found for {} ID: {}", path_segment, media_id);
            }
        }

        Ok(None)
    }

    async fn server_from_query_media_ids(
        &self,
        url: &url::Url,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<Server>> {
        for (param_name, param_value) in url.query_pairs() {
            if !matches_case_insensitive(&param_name, MEDIA_ID_QUERY_TAGS) {
                continue;
            }

            debug!("Found {} in query: {}", param_name, param_value);
            for raw_id in param_value.split(',') {
                let media_id = raw_id.trim();
                if media_id.is_empty() {
                    continue;
                }

                if let Some(server) = self
                    .server_from_client_media_id(media_id, access_scope)
                    .await?
                {
                    debug!(
                        "Found server for {} {}: {} ({})",
                        param_name, media_id, server.name, server.url
                    );
                    return Ok(Some(server));
                }
                debug!("No server found for {}: {}", param_name, media_id);
            }
        }

        Ok(None)
    }

    async fn server_from_client_media_id(
        &self,
        media_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<Server>> {
        if let Some((_mapping, server)) = self
            .data_context
            .media_storage
            .get_media_mapping_with_server(media_id)
            .await?
        {
            if !server_is_allowed(server.id, access_scope, None) {
                return Err(anyhow::anyhow!(
                    "media ID is not available in the current user's server scope"
                ));
            }
            return Ok(Some(server));
        }

        let target = self
            .data_context
            .virtual_library_service
            .routing_target(media_id, access_scope, None)
            .await?;
        if let Some(target) = target {
            return Ok(Some(target.server));
        }

        match self
            .data_context
            .virtual_library_service
            .resolve(media_id, access_scope)
            .await?
        {
            VirtualLibraryResolution::Unknown | VirtualLibraryResolution::Empty { .. } => Ok(None),
            VirtualLibraryResolution::Resolved(_) => Err(anyhow::anyhow!(
                "failed to select a routing target for the resolved virtual library"
            )),
        }
    }
}

fn server_is_allowed(
    server_id: ServerId,
    access_scope: Option<&VirtualLibraryAccessScope>,
    required_server_id: Option<ServerId>,
) -> bool {
    required_server_id.is_none_or(|required| required == server_id)
        && access_scope.is_none_or(|scope| scope.allows(server_id))
}

#[derive(Clone, Copy)]
enum DeliveryUrlStyle {
    Absolute,
    RootRelative,
    Relative,
}

fn parse_delivery_url(value: &str) -> Option<(url::Url, DeliveryUrlStyle)> {
    if let Ok(url) = url::Url::parse(value) {
        return Some((url, DeliveryUrlStyle::Absolute));
    }

    let (path, style) = if value.starts_with('/') {
        (value.to_string(), DeliveryUrlStyle::RootRelative)
    } else {
        (format!("/{value}"), DeliveryUrlStyle::Relative)
    };

    url::Url::parse(&format!("http://localhost{path}"))
        .ok()
        .map(|url| (url, style))
}

fn format_delivery_url(url: url::Url, style: DeliveryUrlStyle) -> String {
    match style {
        DeliveryUrlStyle::Absolute => url.to_string(),
        DeliveryUrlStyle::RootRelative => relative_url_from_parts(&url),
        DeliveryUrlStyle::Relative => relative_url_from_parts(&url)
            .strip_prefix('/')
            .unwrap_or(url.path())
            .to_string(),
    }
}

fn relative_url_from_parts(url: &url::Url) -> String {
    let mut value = url.path().to_string();
    if let Some(query) = url.query() {
        value.push('?');
        value.push_str(query);
    }
    if let Some(fragment) = url.fragment() {
        value.push('#');
        value.push_str(fragment);
    }
    value
}

/// Replace the proxy's own user id wherever it appears as a path segment.
///
/// `USER_ID_PATH_TAGS` finds a user id by the segment in front of it, which only works for paths
/// Jellyfin defines. A plugin addresses its own endpoints however it likes — Jellyfin Enhanced uses
/// `/JellyfinEnhanced/user-settings/{id}/settings.json` and `/JellyfinEnhanced/tag-cache/{id}` —
/// so no list of leading segments can cover them, and extending that list per plugin would put
/// plugin trivia in the routing layer.
///
/// The id itself is the reliable marker. The proxy's user id is meaningless upstream, so a segment
/// equal to it is always one the client took from a proxied response and always wants translating.
/// Matching one known value rather than guessing which segments look like ids also means paths that
/// merely contain an id-shaped segment are untouched. Left in place, the upstream sees one user's
/// id presented with another user's token and answers 403 — the settings a plugin cannot save.
fn replace_proxy_user_id_in_path(url: &mut url::Url, session: &AuthorizationSession) {
    let Some(segments) = url.path_segments() else {
        return;
    };
    let segments: Vec<String> = segments.map(str::to_string).collect();
    if !segments.contains(&session.user_id) {
        return;
    }

    debug!(
        "Replacing proxy user ID in path: {} -> {}",
        session.user_id, session.original_user_id
    );
    let rewritten: Vec<&str> = segments
        .iter()
        .map(|segment| {
            if *segment == session.user_id {
                session.original_user_id.as_str()
            } else {
                segment.as_str()
            }
        })
        .collect();

    if let Ok(mut path) = url.path_segments_mut() {
        path.clear().extend(rewritten);
    }
}

pub fn matches_case_insensitive(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::SqlitePool;

    use super::*;
    use crate::{
        config::{AppConfig, MediaStreamingMode, MIGRATOR},
        media_storage_service::MediaStorageService,
        server_storage::ServerStorageService,
        session_storage::SessionStorage,
        user_authorization_service::{Device, UserAuthorizationService},
        virtual_library_service::VirtualLibraryService,
    };

    #[tokio::test]
    async fn empty_virtual_library_does_not_force_a_routing_server() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let media_storage = MediaStorageService::new(pool.clone());
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let library = virtual_libraries
            .create_group("Empty library")
            .await
            .unwrap();
        let processor = UrlProcessor::new(DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool)),
            server_storage: Arc::new(server_storage),
            media_storage: Arc::new(media_storage),
            virtual_library_service: Arc::new(virtual_libraries),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        });
        let scope = VirtualLibraryAccessScope::new("user", [ServerId::new(1)]);
        let url = url::Url::parse(&format!(
            "http://localhost/Users/user/Items?ParentId={}",
            library.virtual_id
        ))
        .unwrap();

        let server = processor
            .server_from_client_url(&url, Some(&scope))
            .await
            .unwrap();

        assert!(server.is_none());
    }

    #[tokio::test]
    async fn image_tag_remains_opaque_while_item_id_is_remapped() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let server_id = server_storage
            .add_server(
                "Server",
                "http://server.example",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = server_storage
            .get_server_by_id(server_id)
            .await
            .unwrap()
            .unwrap();
        let media_storage = MediaStorageService::new(pool.clone());
        let mapping = media_storage
            .get_or_create_media_mapping("upstream-id", &server)
            .await
            .unwrap();
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let processor = UrlProcessor::new(DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool)),
            server_storage: Arc::new(server_storage),
            media_storage: Arc::new(media_storage),
            virtual_library_service: Arc::new(virtual_libraries),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        });
        let mut url = url::Url::parse(&format!(
            "http://localhost/Items/{}/Images/Primary?tag={}",
            mapping.virtual_media_id, mapping.virtual_media_id
        ))
        .unwrap();

        processor
            .client_to_server_url(&mut url, &None, None, Some(server_id))
            .await;

        assert_eq!(url.path(), "/Items/upstream-id/Images/Primary");
        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key.eq_ignore_ascii_case("tag"))
                .map(|(_, value)| value.into_owned()),
            Some(mapping.virtual_media_id)
        );
    }

    #[tokio::test]
    async fn artist_ids_query_param_is_remapped_to_real_upstream_id() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let server_id = server_storage
            .add_server(
                "Server",
                "http://server.example",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = server_storage
            .get_server_by_id(server_id)
            .await
            .unwrap()
            .unwrap();
        let media_storage = MediaStorageService::new(pool.clone());
        let mapping = media_storage
            .get_or_create_media_mapping("upstream-id", &server)
            .await
            .unwrap();
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let processor = UrlProcessor::new(DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool)),
            server_storage: Arc::new(server_storage),
            media_storage: Arc::new(media_storage),
            virtual_library_service: Arc::new(virtual_libraries),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        });
        let mut url = url::Url::parse(&format!(
            "http://localhost/Items?ContributingArtistIds={}",
            mapping.virtual_media_id
        ))
        .unwrap();

        processor
            .client_to_server_url(&mut url, &None, None, Some(server_id))
            .await;

        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key.eq_ignore_ascii_case("ContributingArtistIds"))
                .map(|(_, value)| value.into_owned()),
            Some(mapping.original_media_id)
        );
    }

    /// `ItemIds` (plural) is used by `/Sessions/{id}/Playing`, `/Playlists/{id}/Items` and the
    /// `/Items` filters. Only the singular `ItemId` was matched, so a virtual id in the plural
    /// form reached the upstream server verbatim and resolved to nothing there.
    #[tokio::test]
    async fn item_ids_query_param_is_remapped_including_comma_separated_lists() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let server_id = server_storage
            .add_server(
                "Server",
                "http://server.example",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = server_storage
            .get_server_by_id(server_id)
            .await
            .unwrap()
            .unwrap();
        let media_storage = MediaStorageService::new(pool.clone());
        let first = media_storage
            .get_or_create_media_mapping("upstream-one", &server)
            .await
            .unwrap();
        let second = media_storage
            .get_or_create_media_mapping("upstream-two", &server)
            .await
            .unwrap();
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let processor = UrlProcessor::new(DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool)),
            server_storage: Arc::new(server_storage),
            media_storage: Arc::new(media_storage),
            virtual_library_service: Arc::new(virtual_libraries),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        });
        let mut url = url::Url::parse(&format!(
            "http://localhost/Sessions/session-id/Playing?itemIds={},{}",
            first.virtual_media_id, second.virtual_media_id
        ))
        .unwrap();

        processor
            .client_to_server_url(&mut url, &None, None, Some(server_id))
            .await;

        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key.eq_ignore_ascii_case("itemIds"))
                .map(|(_, value)| value.into_owned()),
            Some(format!(
                "{},{}",
                first.original_media_id, second.original_media_id
            ))
        );
    }

    /// A playlist is an ordinary item with a virtual id, but `Playlists` was missing from the path
    /// tag list, so `/Playlists/{id}/Items` sent the proxy-minted id upstream where it resolved to
    /// nothing. This affects single-server installs too, not just merged ones.
    #[tokio::test]
    async fn playlist_ids_in_the_path_are_remapped_to_the_upstream_id() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let server_id = server_storage
            .add_server(
                "Server",
                "http://server.example",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = server_storage
            .get_server_by_id(server_id)
            .await
            .unwrap()
            .unwrap();
        let media_storage = MediaStorageService::new(pool.clone());
        let mapping = media_storage
            .get_or_create_media_mapping("upstream-playlist", &server)
            .await
            .unwrap();
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let processor = UrlProcessor::new(DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool)),
            server_storage: Arc::new(server_storage),
            media_storage: Arc::new(media_storage),
            virtual_library_service: Arc::new(virtual_libraries),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        });
        let mut url = url::Url::parse(&format!(
            "http://localhost/Playlists/{}/Items",
            mapping.virtual_media_id
        ))
        .unwrap();

        processor
            .client_to_server_url(&mut url, &None, None, Some(server_id))
            .await;

        assert_eq!(url.path(), "/Playlists/upstream-playlist/Items");
    }

    /// The proxy rewrites `Id` in `/Sessions` responses into its own id space, so a client posting
    /// back to `/Sessions/{id}/Playing` sends an id the upstream never issued. Media Bar's play
    /// button uses exactly that endpoint, and the symptom is a play button that appears to do
    /// nothing — the request 404s and the plugin swallows it.
    #[tokio::test]
    async fn session_ids_in_the_path_are_remapped_to_the_upstream_id() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let server_id = server_storage
            .add_server(
                "Server",
                "http://server.example",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = server_storage
            .get_server_by_id(server_id)
            .await
            .unwrap()
            .unwrap();
        let media_storage = MediaStorageService::new(pool.clone());
        let mapping = media_storage
            .get_or_create_media_mapping("11111111-1111-1111-1111-111111111111", &server)
            .await
            .unwrap();
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let processor = UrlProcessor::new(DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool)),
            server_storage: Arc::new(server_storage),
            media_storage: Arc::new(media_storage),
            virtual_library_service: Arc::new(virtual_libraries),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        });

        let mut url = url::Url::parse(&format!(
            "http://localhost/Sessions/{}/Playing?playCommand=PlayNow",
            mapping.virtual_media_id
        ))
        .unwrap();
        processor
            .client_to_server_url(&mut url, &None, None, Some(server_id))
            .await;
        assert_eq!(
            url.path(),
            format!("/Sessions/{}/Playing", mapping.original_media_id)
        );
    }

    /// `/Sessions` has non-id sub-paths that must not be touched. Only UUID-shaped segments are
    /// treated as ids, so these pass through unchanged.
    #[tokio::test]
    async fn session_sub_paths_that_are_not_ids_are_left_alone() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let media_storage = MediaStorageService::new(pool.clone());
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let processor = UrlProcessor::new(DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool)),
            server_storage: Arc::new(server_storage),
            media_storage: Arc::new(media_storage),
            virtual_library_service: Arc::new(virtual_libraries),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        });

        for path in [
            "/Sessions/Playing",
            "/Sessions/Playing/Progress",
            "/Sessions/Capabilities/Full",
        ] {
            let mut url = url::Url::parse(&format!("http://localhost{path}")).unwrap();
            processor
                .client_to_server_url(&mut url, &None, None, None)
                .await;
            assert_eq!(url.path(), path, "{path} must pass through untouched");
        }
    }

    fn proxy_session() -> AuthorizationSession {
        AuthorizationSession {
            id: 1,
            user_id: "b48bad6ec9f742b8a9cab1cb4e257049".to_string(),
            mapping_id: 1,
            server_url: "http://server.example".to_string(),
            device: Device {
                client: "Jellyfin Media Player".to_string(),
                device: "Gaming".to_string(),
                device_id: "device-id".to_string(),
                version: "1.12.0".to_string(),
            },
            jellyfin_token: "token".to_string(),
            original_user_id: "e44d0fcbabb0453aa24f57d461b986ae".to_string(),
            expires_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    async fn path_through_proxy(path: &str, session: &Option<AuthorizationSession>) -> String {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let media_storage = MediaStorageService::new(pool.clone());
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let processor = UrlProcessor::new(DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool)),
            server_storage: Arc::new(server_storage),
            media_storage: Arc::new(media_storage),
            virtual_library_service: Arc::new(virtual_libraries),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        });

        let mut url = url::Url::parse(&format!("http://localhost{path}")).unwrap();
        processor
            .client_to_server_url(&mut url, session, None, None)
            .await;
        url.path().to_string()
    }

    /// A plugin routes its own endpoints however it likes, so `USER_ID_PATH_TAGS` — which finds a
    /// user id by the segment in front of it — never sees these. Jellyfin Enhanced keeps its
    /// per-user state behind exactly such paths, and an unreplaced proxy id arrives upstream as one
    /// user's id carried by another user's token, which Jellyfin answers 403: settings that will
    /// not save and tags that keep reverting.
    #[tokio::test]
    async fn a_plugin_path_carrying_the_proxy_user_id_is_remapped() {
        let session = Some(proxy_session());

        for (path, expected) in [
            (
                "/JellyfinEnhanced/user-settings/b48bad6ec9f742b8a9cab1cb4e257049/settings.json",
                "/JellyfinEnhanced/user-settings/e44d0fcbabb0453aa24f57d461b986ae/settings.json",
            ),
            (
                "/JellyfinEnhanced/tag-cache/b48bad6ec9f742b8a9cab1cb4e257049",
                "/JellyfinEnhanced/tag-cache/e44d0fcbabb0453aa24f57d461b986ae",
            ),
        ] {
            assert_eq!(path_through_proxy(path, &session).await, expected);
        }
    }

    /// Only the requesting user's own proxy id is replaced. Another id-shaped segment belongs to the
    /// plugin's own data, and guessing that any id in an unknown path is a user reference would
    /// corrupt it.
    #[tokio::test]
    async fn a_plugin_path_without_the_proxy_user_id_is_untouched() {
        let path = "/JellyfinEnhanced/tag-cache/0123456789abcdef0123456789abcdef";
        assert_eq!(
            path_through_proxy(path, &Some(proxy_session())).await,
            path,
            "an id that is not this user's must pass through untouched"
        );
    }

    /// The user id a proxied response hands back is the proxy's, so a client can legitimately place
    /// it anywhere a plugin's API expects a user — including the last segment, and more than once.
    #[tokio::test]
    async fn every_occurrence_of_the_proxy_user_id_is_replaced() {
        let path =
            "/JellyTweaks/b48bad6ec9f742b8a9cab1cb4e257049/peers/b48bad6ec9f742b8a9cab1cb4e257049";
        assert_eq!(
            path_through_proxy(path, &Some(proxy_session())).await,
            "/JellyTweaks/e44d0fcbabb0453aa24f57d461b986ae/peers/e44d0fcbabb0453aa24f57d461b986ae"
        );
    }
}
