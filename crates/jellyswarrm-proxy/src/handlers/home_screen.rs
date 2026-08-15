//! Federating the Home Screen Sections plugin's data across servers.
//!
//! `GET /HomeScreen/Section/{sectionType}` returns a `QueryResult<BaseItemDto>` — the same shape as
//! `/Items` — so the existing fan-out and merge machinery applies unchanged. Without this the
//! request falls through to the catch-all and is answered by whichever single server resolves,
//! which is why every home row showed one server's content while the library beside it showed
//! both.
//!
//! Merging is *not* correct for every section, though, and that is the whole difficulty. Sections
//! divide into two kinds:
//!
//! * **Library-backed** — the plugin builds them by querying its own Jellyfin. Each server computes
//!   its own answer over its own media, so fanning out and merging gives strictly more content than
//!   any single server could, which is the point.
//! * **Externally-backed** — the plugin asks Jellyseerr, Sonarr or Radarr. Every server typically
//!   points at the *same* external service, so fanning out asks one service N times and concatenates
//!   N identical answers. Worse, those items are synthesised and carry no Jellyfin id, so the id
//!   remapper mints a distinct virtual id per copy and deduplication cannot collapse them. The
//!   visible result is every row duplicated.
//!
//! So externally-backed sections are deliberately answered by a single server.

use axum::{
    extract::{Path, State},
    Json,
};
use hyper::StatusCode;
use tracing::debug;

use crate::{
    extractors::Preprocessed,
    handlers::{
        federated::{get_items_from_all_servers_if_not_restricted, get_media_folders},
        items::get_items,
    },
    AppState,
};

/// How a section's data should be obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionSourcing {
    /// Ask every server and merge; each computes its own answer over its own library.
    Federated,
    /// Ask one server. Its answer does not depend on which server is asked.
    SingleServer,
    /// The section lists the user's libraries rather than media, so it has to go through the same
    /// path as `/UserViews`. Merging it as ordinary items returns each server's libraries
    /// separately — "Movies, Movies, Shows, Shows" — while the sidebar beside it shows the merged
    /// set, which looks like duplicates the user cannot get rid of.
    LibraryViews,
}

/// Normalises a section name for comparison against configured lists.
///
/// Section names arrive in whatever case and punctuation the registering plugin chose, so both
/// sides are reduced to lowercase alphanumerics before matching.
fn normalize_section(section_type: &str) -> String {
    section_type
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Decides how a section should be sourced.
///
/// Both lists are configuration rather than constants: which sections are backed by an external
/// service depends on which plugins a deployment runs, and getting it wrong previously required a
/// rebuild to correct. Defaults are in [`crate::config::PluginFederationConfig`]. The single-source
/// list is matched as *prefixes* because plugins name variants by suffix (`Discover`,
/// `DiscoverMovies`, `DiscoverTV`; `UpcomingMovies`, `UpcomingShows`, `UpcomingMusic`).
///
/// Unknown sections default to [`SectionSourcing::Federated`]. A third-party section registered at
/// runtime is far more likely to be library-backed than to wrap an external service, and the cost
/// of the two mistakes is asymmetric: federating a single-source section shows duplicates, which is
/// obvious and reported; single-sourcing a library-backed section silently hides half the library,
/// which is the failure this module exists to remove.
pub fn sourcing_for(
    section_type: &str,
    single_source_prefixes: &[String],
    library_view_sections: &[String],
) -> SectionSourcing {
    let normalized = normalize_section(section_type);

    if library_view_sections
        .iter()
        .any(|name| normalize_section(name) == normalized)
    {
        return SectionSourcing::LibraryViews;
    }

    if single_source_prefixes
        .iter()
        .any(|prefix| normalized.starts_with(&normalize_section(prefix)))
    {
        SectionSourcing::SingleServer
    } else {
        SectionSourcing::Federated
    }
}

/// `GET /HomeScreen/Section/{sectiontype}`
///
/// The path parameter is declared all-lowercase because the routing macro lowercases whole route
/// literals — parameter names included — when it emits its case-insensitive alias, so a camelCase
/// name here fails to bind on the lowercased URL.
pub async fn get_home_screen_section(
    Path(sectiontype): Path<String>,
    state: State<AppState>,
    preprocessed: Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let (single_source_prefixes, library_view_sections) = {
        let config = state.config.read().await;
        (
            config.plugin_federation.single_source_section_prefixes.clone(),
            config.plugin_federation.library_view_sections.clone(),
        )
    };

    match sourcing_for(&sectiontype, &single_source_prefixes, &library_view_sections) {
        SectionSourcing::Federated => {
            debug!("Federating home screen section '{sectiontype}' across all servers");
            get_items_from_all_servers_if_not_restricted(state, preprocessed).await
        }
        SectionSourcing::SingleServer => {
            debug!("Serving home screen section '{sectiontype}' from a single server");
            get_items(state, preprocessed).await
        }
        SectionSourcing::LibraryViews => {
            debug!("Serving home screen section '{sectiontype}' as merged library views");
            // Delegates to the same handler `/Library/MediaFolders` uses, which rewrites the path
            // to `/Users/{id}/Views` and runs the library-grouping plan. That is what collapses two
            // servers' "Movies" into the single merged library the rest of the UI shows.
            get_media_folders(state, preprocessed).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows the user actually reported as broken. Each server computes these over its own
    /// library, so merging is both correct and the entire point.
    #[test]
    fn library_backed_sections_are_federated() {
        for section in [
            "RecentlyAddedMovies",
            "RecentlyAddedShows",
            "RecentlyAddedAlbums",
            "RecentlyAddedArtists",
            "RecentlyAddedBooks",
            "RecentlyAddedAudioBooks",
            "RecentlyAddedMusicVideos",
            "LatestMovies",
            "LatestShows",
            "LatestAlbums",
            "LatestBooks",
            "LatestAudioBooks",
            "LatestMusicVideo",
            "NextUp",
            "ContinueWatching",
            "ContinueWatchingNextUp",
            "WatchAgain",
            "MyList",
            "LiveTV",
        ] {
            assert_eq!(
                sourcing(section),
                SectionSourcing::Federated,
                "{section} reads the local library and must be merged"
            );
        }
    }

    /// Every assertion below is about the *shipped defaults*, so moving these lists into
    /// configuration cannot quietly change what an unconfigured deployment does.
    fn sourcing(section: &str) -> SectionSourcing {
        let defaults = crate::config::PluginFederationConfig::default();
        sourcing_for(
            section,
            &defaults.single_source_section_prefixes,
            &defaults.library_view_sections,
        )
    }

    /// Every server points at the same Jellyseerr or Arr instance, so asking all of them returns
    /// the same list several times. The items carry no Jellyfin id, so nothing downstream can
    /// collapse the copies and the row renders duplicated.
    #[test]
    fn externally_backed_sections_are_answered_by_one_server() {
        for section in [
            "Discover",
            "DiscoverMovies",
            "DiscoverTV",
            "MyJellyseerrRequests",
            "UpcomingMovies",
            "UpcomingShows",
            "UpcomingBooks",
            "UpcomingMusic",
        ] {
            assert_eq!(
                sourcing(section),
                SectionSourcing::SingleServer,
                "{section} comes from an external service and must not be fanned out"
            );
        }
    }

    /// Section ids arrive from a client and from third-party registrations, so matching cannot
    /// depend on exact casing or punctuation.
    #[test]
    fn matching_ignores_case_and_punctuation() {
        assert_eq!(sourcing("discovermovies"), SectionSourcing::SingleServer);
        assert_eq!(sourcing("DISCOVER_MOVIES"), SectionSourcing::SingleServer);
        assert_eq!(sourcing("recentlyaddedmovies"), SectionSourcing::Federated);
    }

    /// An unanticipated section federates. The two possible mistakes are not equally bad: wrongly
    /// federating shows visible duplicates, while wrongly single-sourcing silently hides half the
    /// library — which is precisely the bug being fixed.
    #[test]
    fn an_unknown_section_defaults_to_federating() {
        assert_eq!(
            sourcing("SomeThirdPartySection"),
            SectionSourcing::Federated
        );
        assert_eq!(sourcing(""), SectionSourcing::Federated);
    }

    /// My Media lists libraries, not media. Merging it as items shows each server's libraries
    /// separately while the sidebar shows the merged set, so the same library appears twice with no
    /// way for the user to collapse it.
    #[test]
    fn my_media_is_resolved_as_library_views() {
        assert_eq!(sourcing("MyMedia"), SectionSourcing::LibraryViews);
        assert_eq!(sourcing("mymedia"), SectionSourcing::LibraryViews);
    }

    /// Prefix matching must not catch a longer word that merely starts the same way.
    #[test]
    fn prefix_matching_does_not_overreach() {
        // "Discovery" style names are still external-family and should single-source...
        assert_eq!(sourcing("DiscoverAnime"), SectionSourcing::SingleServer);
        // ...but an unrelated section that happens to contain the word is not a prefix match.
        assert_eq!(sourcing("RecentlyDiscovered"), SectionSourcing::Federated);
    }

    /// The point of moving these out of Rust: an operator can add a section the proxy has never
    /// heard of without a rebuild, and it takes effect.
    #[test]
    fn a_configured_prefix_changes_how_a_section_is_sourced() {
        let single = vec!["mycustomarr".to_string()];
        let views = vec!["mymedia".to_string()];

        assert_eq!(
            sourcing_for("MyCustomArrUpcoming", &single, &views),
            SectionSourcing::SingleServer,
            "a newly configured prefix must take effect"
        );
        assert_eq!(
            sourcing_for("Discover", &single, &views),
            SectionSourcing::Federated,
            "a prefix removed from the list must stop single-sourcing"
        );
        assert_eq!(
            sourcing_for("MyMedia", &single, &[]),
            SectionSourcing::Federated,
            "clearing the library-view list must stop treating My Media specially"
        );
    }
}
