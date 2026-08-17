use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use serde_json::{Map, Value};
use tracing::{debug, error};

use crate::{
    media_storage_service::MediaStorageService,
    processors::{
        field_matcher::{
            DELIVERY_URL_FIELDS, DISABLED_BOOL_FIELDS, MEDIA_ID_ARRAY_FIELDS,
            MEDIA_ID_MAP_KEY_FIELDS, MEDIA_ID_MAP_VALUE_FIELDS, MEDIA_ID_NESTED_MAP_KEY_FIELDS,
            NAME_FIELDS, RESPONSE_MEDIA_ID_FIELDS, SERVER_ID_FIELDS,
        },
        json_processor::{JsonProcessingContext, JsonProcessingResult, JsonProcessor},
        url_processor::UrlProcessor,
    },
    server_storage::Server,
    url_helper::is_id_like,
    DataContext,
};

pub struct ResponseProcessor {
    pub data_context: DataContext,
    url_processor: UrlProcessor,
}

impl ResponseProcessor {
    pub fn new(data_context: DataContext) -> Self {
        Self {
            url_processor: UrlProcessor::new(data_context.clone()),
            data_context,
        }
    }

    async fn virtual_media_id(&self, id: &str, server: &Server) -> Result<String, String> {
        self.data_context
            .media_storage
            .get_or_create_media_mapping(id, server)
            .await
            .map(|mapping| mapping.virtual_media_id)
            .map_err(|e| format!("failed to create media mapping for {id}: {e}"))
    }

    async fn remap_delivery_url(
        &self,
        value: &str,
        context: &ResponseProcessingContext,
    ) -> Result<Option<String>, String> {
        self.url_processor
            .server_to_client_delivery_url(value, &context.server, context.proxy_api_key.as_deref())
            .await
            .map_err(|e| e.to_string())
    }
}

pub struct ResponseProcessingContext {
    pub server: Server,
    pub proxy_server_id: String,
    pub proxy_api_key: Option<String>,
    pub profile: ResponseProcessingProfile,
    pub should_change_name: bool,
    pub can_change_item_names: bool,
    /// Object keys in this payload that are known media ids of `server`, mapped to their virtual
    /// ids. Resolved up front by [`media_id_object_keys`] because the ids appear as *keys*, and a
    /// key can only be renamed by a caller that already knows what to rename it to.
    pub media_id_object_keys: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseProcessingProfile {
    Media,
    BestEffortMedia,
    Disabled,
}

impl ResponseProcessingContext {
    fn rewrites_media_fields(&self) -> bool {
        matches!(
            self.profile,
            ResponseProcessingProfile::Media | ResponseProcessingProfile::BestEffortMedia
        )
    }
}

#[async_trait]
impl JsonProcessor<ResponseProcessingContext> for ResponseProcessor {
    async fn process(
        &self,
        json_context: &JsonProcessingContext,
        value: &mut Value,
        context: &ResponseProcessingContext,
    ) -> JsonProcessingResult {
        let mut result = JsonProcessingResult::new();

        if context.profile == ResponseProcessingProfile::Disabled {
            return result;
        }

        if context.rewrites_media_fields()
            && json_context.is_array_item
            && MEDIA_ID_ARRAY_FIELDS.contains(last_segment(&json_context.parent_path))
        {
            if let Some(id) = value.as_str().map(str::to_string) {
                match self.virtual_media_id(&id, &context.server).await {
                    Ok(virtual_id) => {
                        debug!("Replacing response array media ID {} -> {}", id, virtual_id);
                        *value = Value::String(virtual_id);
                        result = result.mark_modified();
                    }
                    Err(e) => result = result.add_error(e),
                }
            }
            return result;
        }

        if context.rewrites_media_fields() && should_remap_map_value(&json_context.parent_path) {
            if let Some(id) = value.as_str().map(str::to_string) {
                match self.virtual_media_id(&id, &context.server).await {
                    Ok(virtual_id) => {
                        debug!("Replacing response map media ID {} -> {}", id, virtual_id);
                        *value = Value::String(virtual_id);
                        result = result.mark_modified();
                    }
                    Err(e) => result = result.add_error(e),
                }
            }
            return result;
        }

        if context.rewrites_media_fields() && should_remap_map_key(&json_context.parent_path) {
            match self
                .virtual_media_id(&json_context.key, &context.server)
                .await
            {
                Ok(virtual_id) => {
                    debug!(
                        "Replacing response map key media ID {} -> {}",
                        json_context.key, virtual_id
                    );
                    result = result.rename_key(virtual_id);
                }
                Err(e) => result = result.add_error(e),
            }
            return result;
        }

        if context.rewrites_media_fields() {
            if let Some(virtual_id) = context
                .media_id_object_keys
                .get(&MediaStorageService::normalize_uuid(&json_context.key))
            {
                debug!(
                    "Replacing plugin response object key media ID {} -> {}",
                    json_context.key, virtual_id
                );
                return result.rename_key(virtual_id.clone());
            }
        }

        if context.rewrites_media_fields()
            && RESPONSE_MEDIA_ID_FIELDS.contains(&json_context.key)
            && !is_legacy_unmapped_media_id_field(json_context)
        {
            if let Some(id) = value.as_str().map(str::to_string) {
                match self.virtual_media_id(&id, &context.server).await {
                    Ok(virtual_id) => {
                        debug!(
                            "Replacing response media ID {} -> {} for field {}",
                            id, virtual_id, json_context.key
                        );
                        *value = Value::String(virtual_id);
                        result = result.mark_modified();
                    }
                    Err(e) => result = result.add_error(e),
                }
            }
        } else if DELIVERY_URL_FIELDS.contains(&json_context.key) {
            if let Some(delivery_url) = value.as_str().map(str::to_string) {
                match self.remap_delivery_url(&delivery_url, context).await {
                    Ok(Some(remapped)) => {
                        *value = Value::String(remapped);
                        result = result.mark_modified();
                    }
                    Ok(None) => {}
                    Err(e) => result = result.add_error(e),
                }
            }
        } else if context.rewrites_media_fields()
            && DISABLED_BOOL_FIELDS.contains(&json_context.key)
        {
            if value.is_boolean() {
                *value = Value::Bool(false);
                result = result.mark_modified();
            }
        } else if context.rewrites_media_fields() && SERVER_ID_FIELDS.contains(&json_context.key) {
            if value.is_string() {
                *value = Value::String(context.proxy_server_id.clone());
                result = result.mark_modified();
            }
        } else if context.rewrites_media_fields() && should_change_name(json_context, context) {
            if let Value::String(name) = value {
                *name = format!("{} [{}]", name, context.server.name);
                result = result.mark_modified();
            }
        }

        result
    }
}

/// Resolve every object key in `payload` that is a known media id of `server`.
///
/// `MEDIA_ID_MAP_KEY_FIELDS` finds id-keyed maps by the field that encloses them, which works for
/// the shapes Jellyfin defines and not at all for the ones a plugin invents. Jellyfin Enhanced
/// keys its tag cache directly by item id, so its keys sit under no recognised field and survived
/// untouched: five figures of entries in the upstream's id space, addressed by a page holding
/// virtual ids, matching nothing and rendering no badges at all.
///
/// Recognition is by lookup rather than by shape. An id-shaped key is only a media id if the proxy
/// has already mapped it for this server, and only then can it correspond to something on screen.
/// Anything else — a plugin's own identifiers, a hash, an id belonging to another server — has no
/// mapping and is left exactly as it is.
pub async fn media_id_object_keys(
    media_storage: &MediaStorageService,
    payload: &serde_json::Value,
    server: &Server,
) -> HashMap<String, String> {
    let mut candidates = HashSet::new();
    collect_id_like_object_keys(payload, &mut candidates);
    if candidates.is_empty() {
        return HashMap::new();
    }

    let candidates: Vec<String> = candidates.into_iter().collect();
    match media_storage
        .virtual_ids_for_originals(&candidates, server.id)
        .await
    {
        Ok(resolved) => resolved,
        Err(e) => {
            error!("Failed to resolve media ids used as object keys: {e}");
            HashMap::new()
        }
    }
}

fn collect_id_like_object_keys(value: &serde_json::Value, out: &mut HashSet<String>) {
    match value {
        Value::Object(entries) => {
            for (key, child) in entries {
                if is_id_like(key) {
                    out.insert(MediaStorageService::normalize_uuid(key));
                }
                collect_id_like_object_keys(child, out);
            }
        }
        Value::Array(entries) => {
            for entry in entries {
                collect_id_like_object_keys(entry, out);
            }
        }
        _ => {}
    }
}

fn should_remap_map_value(parent_path: &str) -> bool {
    MEDIA_ID_MAP_VALUE_FIELDS.contains(last_segment(parent_path))
}

fn should_remap_map_key(parent_path: &str) -> bool {
    MEDIA_ID_MAP_KEY_FIELDS.contains(last_segment(parent_path))
        || parent_contains_nested_map_key_field(parent_path)
}

fn parent_contains_nested_map_key_field(parent_path: &str) -> bool {
    let mut seen_nested_map = false;
    for segment in path_segments(parent_path) {
        if MEDIA_ID_NESTED_MAP_KEY_FIELDS.contains(segment) {
            seen_nested_map = true;
            continue;
        }

        if seen_nested_map {
            return true;
        }
    }

    false
}

fn is_legacy_unmapped_media_id_field(json_context: &JsonProcessingContext) -> bool {
    let is_user_data_item_id = json_context.key.eq_ignore_ascii_case("ItemId")
        && path_segments(&json_context.parent_path)
            .any(|segment| segment.eq_ignore_ascii_case("UserData"));
    let is_media_source_etag = json_context.key.eq_ignore_ascii_case("Etag")
        && path_segments(&json_context.parent_path)
            .any(|segment| segment.eq_ignore_ascii_case("MediaSources"));

    is_user_data_item_id || is_media_source_etag
}

fn should_change_name(
    json_context: &JsonProcessingContext,
    context: &ResponseProcessingContext,
) -> bool {
    context.should_change_name
        && context.can_change_item_names
        && NAME_FIELDS.contains(&json_context.key)
        && is_media_item_root_path(&json_context.parent_path)
        && !is_live_tv_item(json_context.parent_object.as_ref())
}

fn is_media_item_root_path(parent_path: &str) -> bool {
    parent_path.is_empty()
        || (parent_path.starts_with('[') && !parent_path.contains('.'))
        || last_segment(parent_path).eq_ignore_ascii_case("Items")
}

fn is_live_tv_item(parent_object: Option<&Map<String, Value>>) -> bool {
    let Some(parent_object) = parent_object else {
        return false;
    };

    parent_object
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("CollectionType"))
        .and_then(|(_, value)| value.as_str())
        .is_some_and(|collection_type| collection_type.eq_ignore_ascii_case("LiveTv"))
}

fn last_segment(path: &str) -> &str {
    path.rsplit('.')
        .next()
        .map(strip_array_index)
        .unwrap_or(path)
}

fn path_segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('.').map(strip_array_index)
}

fn strip_array_index(segment: &str) -> &str {
    segment.split('[').next().unwrap_or(segment)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use sqlx::SqlitePool;

    use super::*;
    use crate::{
        config::{AppConfig, MediaStreamingMode, MIGRATOR},
        server_storage::ServerStorageService,
        session_storage::SessionStorage,
        user_authorization_service::UserAuthorizationService,
        virtual_library_service::VirtualLibraryService,
    };

    /// Runs `payload` through response processing against a server holding one already-mapped item,
    /// returning the rewritten payload and that item's virtual id.
    async fn process(payload: &mut Value, mapped_id: &str) -> String {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let server_id = server_storage
            .add_server(
                "SGTV",
                "http://sgtv.example",
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
            .get_or_create_media_mapping(mapped_id, &server)
            .await
            .unwrap();
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let data_context = DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool)),
            server_storage: Arc::new(server_storage),
            media_storage: Arc::new(media_storage),
            virtual_library_service: Arc::new(virtual_libraries),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        };

        let context = ResponseProcessingContext {
            server: server.clone(),
            proxy_server_id: "proxy".to_string(),
            proxy_api_key: None,
            profile: ResponseProcessingProfile::Media,
            should_change_name: false,
            can_change_item_names: false,
            media_id_object_keys: media_id_object_keys(
                &data_context.media_storage.clone(),
                payload,
                &server,
            )
            .await,
        };

        let processor = ResponseProcessor::new(data_context);
        let processed =
            crate::processors::json_processor::process_json(payload, &processor, &context)
                .await
                .unwrap();
        *payload = processed.data;
        mapping.virtual_media_id
    }

    /// Jellyfin Enhanced keys its tag cache directly by item id, under no field the proxy
    /// recognises. Left in the upstream's id space the entries match nothing on a page built from
    /// virtual ids, and every quality and rating badge silently fails to render.
    #[tokio::test]
    async fn media_ids_used_as_object_keys_are_remapped() {
        let original = "11111111-1111-1111-1111-111111111111";
        let mut payload = json!({
            "version": 1,
            "items": {
                original.replace('-', ""): { "quality": "1080P", "rating": 7.4 },
            },
        });

        let virtual_id = process(&mut payload, original).await;

        let items = payload["items"].as_object().unwrap();
        assert!(
            items.contains_key(&virtual_id),
            "the tag entry must be addressable by the id the page actually holds"
        );
        assert_eq!(items[&virtual_id]["quality"], "1080P");
    }

    /// Recognition is by lookup, not by shape: an id the proxy has never mapped cannot be on screen,
    /// and may not even be a media id. Rewriting it — or minting a mapping to make one — would put
    /// the plugin's own identifiers into the proxy's id space.
    #[tokio::test]
    async fn id_shaped_keys_without_a_mapping_are_left_alone() {
        let unmapped = "22222222222222222222222222222222";
        let mut payload = json!({ unmapped: { "quality": "4K" } });

        process(&mut payload, "11111111-1111-1111-1111-111111111111").await;

        assert!(
            payload.as_object().unwrap().contains_key(unmapped),
            "an unmapped id-shaped key must survive untouched"
        );
    }

    /// A plugin is free to nest, so keying off the enclosing field is what failed in the first
    /// place. Depth must not matter.
    #[tokio::test]
    async fn media_id_keys_are_remapped_at_any_depth() {
        let original = "11111111-1111-1111-1111-111111111111";
        let mut payload = json!({
            "cache": { "byUser": [ { original.replace('-', ""): { "rating": 9.1 } } ] },
        });

        let virtual_id = process(&mut payload, original).await;

        let nested = payload["cache"]["byUser"][0].as_object().unwrap();
        assert!(nested.contains_key(&virtual_id));
    }
}
