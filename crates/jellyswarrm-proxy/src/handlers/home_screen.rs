use axum::{extract::State, Json};
use hyper::StatusCode;

use crate::{
    extractors::Preprocessed, handlers::federated::get_items_from_all_servers_if_not_restricted,
    AppState,
};

/// `GET /HomeScreen/Section/{name}` — the Home Screen Sections plugin
/// (IAmParadox27/jellyfin-plugin-home-sections) serves each section's data as a
/// standard `QueryResult<BaseItemDto>`, the same shape as `/Items`. Every upstream
/// server runs its own copy of the plugin, so we fan the request out to all of the
/// user's servers, let each compute its own section, and merge the results —
/// remapping item ids into virtual space — so a section shows combined content from
/// every server instead of only the primary one.
///
/// Delegating to `get_items_from_all_servers_if_not_restricted` also means a
/// library-scoped section whose `ParentId` is a virtual merged-library id is expanded
/// into the correct per-server library ids before fan-out, and a section pinned to a
/// single library/series still resolves to the one server that owns it.
pub async fn get_home_screen_section(
    state: State<AppState>,
    preprocessed: Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    get_items_from_all_servers_if_not_restricted(state, preprocessed).await
}
