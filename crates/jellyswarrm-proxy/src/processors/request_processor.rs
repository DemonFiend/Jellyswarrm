use async_trait::async_trait;
use serde_json::Value;
use tracing::{debug, info};

use crate::processors::field_matcher::{ID_FIELDS, SESSION_FIELDS, USER_FIELDS};
use crate::processors::json_processor::{
    JsonProcessingContext, JsonProcessingResult, JsonProcessor,
};
use crate::request_preprocessing::{JellyfinAuthorization, PreprocessedRequest};
use crate::server_storage::Server;
use crate::user_authorization_service::{AuthorizationSession, User};
use crate::DataContext;

pub struct RequestProcessor {
    pub data_context: DataContext,
}

impl RequestProcessor {
    pub fn new(data_context: DataContext) -> Self {
        Self { data_context }
    }

    /// Resolves a virtual id back to the upstream id, but only when it belongs to the server this
    /// request is being sent to. An id owned by a different server is left alone rather than
    /// rewritten into something meaningless for the target.
    async fn resolve_virtual_id(
        &self,
        virtual_id: &str,
        target_server: crate::server_id::ServerId,
    ) -> Option<String> {
        let mapping = self
            .data_context
            .media_storage
            .get_media_mapping_by_virtual(virtual_id)
            .await
            .unwrap_or_default()?;

        (mapping.server_id == target_server).then_some(mapping.original_media_id)
    }
}

#[allow(dead_code)]
pub struct RequestProcessingContext {
    pub user: Option<User>,
    pub server: Server,
    pub sessions: Option<Vec<(AuthorizationSession, Server)>>,
    pub auth: Option<JellyfinAuthorization>,
    pub session: Option<AuthorizationSession>,
    pub new_auth: Option<JellyfinAuthorization>,
}

impl RequestProcessingContext {
    pub fn new(preprocessed_request: &PreprocessedRequest) -> Self {
        Self {
            user: preprocessed_request.user.clone(),
            server: preprocessed_request.server.clone(),
            sessions: preprocessed_request.sessions.clone(),
            auth: preprocessed_request.auth.clone(),
            session: preprocessed_request.session.clone(),
            new_auth: preprocessed_request.new_auth.clone(),
        }
    }
}

#[async_trait]
impl JsonProcessor<RequestProcessingContext> for RequestProcessor {
    async fn process(
        &self,
        json_context: &JsonProcessingContext,
        value: &mut Value,
        context: &RequestProcessingContext,
    ) -> JsonProcessingResult {
        let mut result = JsonProcessingResult::new();
        // Check if this is an ID field (case-insensitive)
        if ID_FIELDS.contains(&json_context.key) {
            match value {
                Value::String(ref virtual_id) => {
                    if let Some(original_id) =
                        self.resolve_virtual_id(virtual_id, context.server.id).await
                    {
                        debug!(
                            "Replacing virtual id {} -> {} for field: {} in payload",
                            virtual_id, &original_id, &json_context.key
                        );
                        *value = Value::String(original_id);
                        result = result.mark_modified();
                    }
                }
                // Plural id fields such as `Ids` on `POST /Playlists` carry an array of ids. The
                // walker descends into arrays but only hands objects back to the processor, so
                // string elements were never remapped and the proxy's own virtual ids were
                // persisted upstream — every playlist created through the proxy then referred to
                // items the upstream server had never heard of.
                Value::Array(ref mut items) => {
                    for item in items.iter_mut() {
                        let Value::String(ref virtual_id) = item else {
                            continue;
                        };
                        if let Some(original_id) =
                            self.resolve_virtual_id(virtual_id, context.server.id).await
                        {
                            debug!(
                                "Replacing virtual id {} -> {} in array field: {}",
                                virtual_id, &original_id, &json_context.key
                            );
                            *item = Value::String(original_id);
                            result = result.mark_modified();
                        }
                    }
                }
                _ => {}
            }
        }
        // Handle session IDs that might need transformation
        else if SESSION_FIELDS.contains(&json_context.key) {
            // For requests, session IDs typically stay as-is
        }
        // Handle user IDs
        else if USER_FIELDS.contains(&json_context.key) {
            if let Value::String(ref virtual_id) = value {
                // For requests, we need to convert virtual IDs back to real IDs
                if let Some(session) = &context.session {
                    info!(
                        "Replacing User ID {} -> {} for field: {} in payload",
                        virtual_id, &session.original_user_id, &json_context.key
                    );
                    *value = Value::String(session.original_user_id.clone());
                    result = result.mark_modified();
                }
            }
        }
        // Handle any other request-specific transformations
        else {
            // Handle any other request-specific transformations
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use sqlx::SqlitePool;

    use super::*;
    use crate::{
        config::{AppConfig, MediaStreamingMode, MIGRATOR},
        media_storage_service::MediaStorageService,
        processors::process_json,
        server_id::ServerId,
        server_storage::ServerStorageService,
        server_url::ServerUrl,
        session_storage::SessionStorage,
        user_authorization_service::{Device, UserAuthorizationService},
        virtual_library_service::VirtualLibraryService,
    };

    async fn test_data_context() -> DataContext {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let media_storage = MediaStorageService::new(pool.clone());

        DataContext {
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
        }
    }

    fn test_server() -> Server {
        let now = chrono::Utc::now();
        Server {
            id: ServerId::new(1),
            name: "Test Server".to_string(),
            url: ServerUrl::parse("http://server.example:8096").unwrap(),
            priority: 0,
            media_streaming_mode: MediaStreamingMode::Redirect,
            created_at: now,
            updated_at: now,
        }
    }

    fn test_session() -> AuthorizationSession {
        let now = chrono::Utc::now();
        AuthorizationSession {
            id: 1,
            user_id: "proxy-user".to_string(),
            mapping_id: 1,
            server_url: "http://server.example:8096".to_string(),
            device: Device {
                client: "Test".to_string(),
                device: "Test Device".to_string(),
                device_id: "device-id".to_string(),
                version: "1".to_string(),
            },
            jellyfin_token: "server-token".to_string(),
            original_user_id: "upstream-user".to_string(),
            expires_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn user_id_rewrite_marks_request_body_modified() {
        let processor = RequestProcessor::new(test_data_context().await);
        let context = RequestProcessingContext {
            user: None,
            server: test_server(),
            sessions: None,
            auth: None,
            session: Some(test_session()),
            new_auth: None,
        };
        let mut payload = json!({ "UserId": "proxy-user" });

        let response = process_json(&mut payload, &processor, &context)
            .await
            .unwrap();

        assert!(response.was_modified);
        assert_eq!(payload["UserId"], "upstream-user");
    }

    /// `POST /Playlists` sends `{"Ids": ["...", "..."]}`. The JSON walker descends into arrays but
    /// only hands objects back to the processor, so string elements were never remapped and the
    /// proxy's own virtual ids were persisted upstream — every playlist created through the proxy
    /// then referenced items the upstream server had never heard of.
    #[tokio::test]
    async fn id_arrays_in_request_bodies_are_remapped_to_upstream_ids() {
        let data_context = test_data_context().await;
        let server = test_server();
        data_context
            .server_storage
            .add_server(
                &server.name,
                server.url.as_str(),
                server.priority,
                server.media_streaming_mode,
            )
            .await
            .unwrap();
        let first = data_context
            .media_storage
            .get_or_create_media_mapping("upstream-one", &server)
            .await
            .unwrap();
        let second = data_context
            .media_storage
            .get_or_create_media_mapping("upstream-two", &server)
            .await
            .unwrap();

        let processor = RequestProcessor::new(data_context);
        let context = RequestProcessingContext {
            user: None,
            server: test_server(),
            sessions: None,
            auth: None,
            session: Some(test_session()),
            new_auth: None,
        };
        let mut payload = json!({
            "Name": "My Playlist",
            "Ids": [first.virtual_media_id, second.virtual_media_id],
        });

        let response = process_json(&mut payload, &processor, &context)
            .await
            .unwrap();

        assert!(response.was_modified);
        assert_eq!(payload["Ids"][0], "upstream-one");
        assert_eq!(payload["Ids"][1], "upstream-two");
        assert_eq!(payload["Name"], "My Playlist", "other fields untouched");
    }
}
