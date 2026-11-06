//! Admin controls for how per-server plugin surfaces are combined.
//!
//! Client-side plugins keep their configuration on each media server, so each one raises the same
//! question: when several servers disagree, whose answer does the browser get? These settings were
//! previously constants in three modules, which meant answering that question required a rebuild
//! and gave an operator no way to see what the current policy even was.
//!
//! Everything here is read per request, so a change takes effect on the next page load without a
//! restart.

use askama::Template;
use axum::{
    extract::State,
    response::{Html, IntoResponse, Response},
};
use hyper::StatusCode;
use tracing::error;

use crate::{
    config::{save_config, CustomTabsMode, ProxyCustomTab},
    AppState,
};

#[derive(Template)]
#[template(path = "admin/plugin_federation.html")]
pub struct PluginFederationTemplate {
    pub ui_route: String,
    pub mode_pinned: bool,
    pub mode_merged: bool,
    pub mode_proxy: bool,
    pub tabs: Vec<ProxyCustomTab>,
    pub plugin_asset_prefixes: String,
    pub plugin_api_prefixes: String,
    pub federated_plugin_api_prefixes: String,
    pub single_source_section_prefixes: String,
    pub library_view_sections: String,
    pub message: String,
}

/// Splits a textarea into entries, one per line, discarding blanks and surrounding space.
///
/// Commas are accepted too, because these values are written as comma-separated lists everywhere
/// else they appear (config files, documentation) and silently keeping `"/A, /B"` as a single
/// entry would look like the setting had simply been ignored.
pub fn parse_lines(value: &str) -> Vec<String> {
    value
        .split(['\n', '\r', ','])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

/// Renders a list back into the textarea form, one entry per line.
fn join_lines(values: &[String]) -> String {
    values.join("\n")
}

#[derive(Debug, PartialEq, Eq)]
pub struct FederationForm {
    pub mode: CustomTabsMode,
    pub tabs: Vec<ProxyCustomTab>,
    pub plugin_asset_prefixes: Vec<String>,
    pub plugin_api_prefixes: Vec<String>,
    pub federated_plugin_api_prefixes: Vec<String>,
    pub single_source_section_prefixes: Vec<String>,
    pub library_view_sections: Vec<String>,
}

/// Parses the settings form.
///
/// Hand-rolled rather than using axum's `Form` for the same reason the plugin install form is: the
/// tab editor submits one `tab_title`/`tab_html` pair per row, and `Form` cannot deserialize
/// repeated fields into a `Vec` — it rejects the whole body instead, which fails at runtime rather
/// than at compile time.
///
/// Titles and bodies are paired **by position**, so a row whose title is blank is dropped along
/// with its body. That is what makes the trailing empty row in the editor harmless.
pub fn parse_federation_form(body: &str) -> FederationForm {
    let mut mode = CustomTabsMode::Pinned;
    let mut titles: Vec<String> = Vec::new();
    let mut bodies: Vec<String> = Vec::new();
    let mut plugin_asset_prefixes = Vec::new();
    let mut plugin_api_prefixes = Vec::new();
    let mut federated_plugin_api_prefixes = Vec::new();
    let mut single_source_section_prefixes = Vec::new();
    let mut library_view_sections = Vec::new();

    for (key, value) in url::form_urlencoded::parse(body.as_bytes()) {
        match key.as_ref() {
            "custom_tabs_mode" => {
                mode = match value.as_ref() {
                    "merged" => CustomTabsMode::Merged,
                    "proxy" => CustomTabsMode::Proxy,
                    // Anything unrecognised keeps the behaviour that existed before this setting,
                    // rather than inventing a policy from a malformed request.
                    _ => CustomTabsMode::Pinned,
                }
            }
            "tab_title" => titles.push(value.trim().to_string()),
            "tab_html" => bodies.push(value.into_owned()),
            "plugin_asset_prefixes" => plugin_asset_prefixes = parse_lines(&value),
            "plugin_api_prefixes" => plugin_api_prefixes = parse_lines(&value),
            "federated_plugin_api_prefixes" => federated_plugin_api_prefixes = parse_lines(&value),
            "single_source_section_prefixes" => {
                single_source_section_prefixes = parse_lines(&value)
            }
            "library_view_sections" => library_view_sections = parse_lines(&value),
            _ => {}
        }
    }

    let tabs = titles
        .into_iter()
        .enumerate()
        .filter(|(_, title)| !title.is_empty())
        .map(|(index, title)| ProxyCustomTab {
            title,
            content_html: bodies.get(index).cloned().unwrap_or_default(),
        })
        .collect();

    FederationForm {
        mode,
        tabs,
        plugin_asset_prefixes,
        plugin_api_prefixes,
        federated_plugin_api_prefixes,
        single_source_section_prefixes,
        library_view_sections,
    }
}

/// Builds the form from the live configuration, plus one blank tab row to type into.
async fn render(state: &AppState, message: String) -> Response {
    let (ui_route, federation) = {
        let config = state.config.read().await;
        (
            config.ui_route.to_string(),
            config.plugin_federation.clone(),
        )
    };

    let mut tabs = federation.custom_tabs.clone();
    tabs.push(ProxyCustomTab {
        title: String::new(),
        content_html: String::new(),
    });

    let template = PluginFederationTemplate {
        ui_route,
        mode_pinned: federation.custom_tabs_mode == CustomTabsMode::Pinned,
        mode_merged: federation.custom_tabs_mode == CustomTabsMode::Merged,
        mode_proxy: federation.custom_tabs_mode == CustomTabsMode::Proxy,
        tabs,
        plugin_asset_prefixes: join_lines(&federation.plugin_asset_prefixes),
        plugin_api_prefixes: join_lines(&federation.plugin_api_prefixes),
        federated_plugin_api_prefixes: join_lines(&federation.federated_plugin_api_prefixes),
        single_source_section_prefixes: join_lines(&federation.single_source_section_prefixes),
        library_view_sections: join_lines(&federation.library_view_sections),
        message,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(error) => {
            error!("Failed to render the plugin federation form: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /{ui}/plugins/federation`
pub async fn federation_form(State(state): State<AppState>) -> Response {
    render(&state, String::new()).await
}

/// `POST /{ui}/plugins/federation`
pub async fn save_federation(State(state): State<AppState>, body: String) -> Response {
    let form = parse_federation_form(&body);

    // An empty asset list would stop relaying every plugin route at once, taking the plugin UIs
    // down with no obvious cause. Refuse rather than save it.
    if form.plugin_asset_prefixes.is_empty() {
        return render(
            &state,
            "<div class=\"alert alert-error\">Plugin asset routes cannot be empty &mdash; that would stop every client-side plugin from loading. Reset the field to restore the defaults.</div>".to_string(),
        )
        .await;
    }

    let save_result = {
        let mut config = state.config.write().await;
        let mut updated = config.clone();
        updated.plugin_federation.custom_tabs_mode = form.mode;
        updated.plugin_federation.custom_tabs = form.tabs;
        updated.plugin_federation.plugin_asset_prefixes = form.plugin_asset_prefixes;
        updated.plugin_federation.plugin_api_prefixes = form.plugin_api_prefixes;
        updated.plugin_federation.federated_plugin_api_prefixes =
            form.federated_plugin_api_prefixes;
        updated.plugin_federation.single_source_section_prefixes =
            form.single_source_section_prefixes;
        updated.plugin_federation.library_view_sections = form.library_view_sections;

        match save_config(&updated) {
            Ok(()) => {
                *config = updated;
                Ok(())
            }
            Err(error) => Err(error),
        }
    };

    match save_result {
        Ok(()) => {
            render(
                &state,
                "<div class=\"alert\">Saved. Reload the web client to see the change.</div>"
                    .to_string(),
            )
            .await
        }
        Err(error) => {
            error!("Failed to save plugin federation settings: {error}");
            render(
                &state,
                "<div class=\"alert alert-error\">Could not save. The previous settings are still active.</div>".to_string(),
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(title: &str, html: &str) -> ProxyCustomTab {
        ProxyCustomTab {
            title: title.to_string(),
            content_html: html.to_string(),
        }
    }

    /// The editor always submits a trailing blank row to type into. It must not become a tab.
    #[test]
    fn a_blank_row_is_not_saved_as_a_tab() {
        let form = parse_federation_form(
            "custom_tabs_mode=proxy&tab_title=Requests&tab_html=%3Cb%3Ehi%3C%2Fb%3E&tab_title=&tab_html=&plugin_asset_prefixes=%2FCustomTabs",
        );

        assert_eq!(form.tabs, vec![tab("Requests", "<b>hi</b>")]);
    }

    /// Titles and bodies are paired by position, so a blank row in the *middle* must not shift
    /// every later body onto the wrong title.
    #[test]
    fn a_blank_row_between_two_tabs_does_not_shift_the_others() {
        let form = parse_federation_form(
            "tab_title=One&tab_html=first&tab_title=&tab_html=&tab_title=Two&tab_html=second",
        );

        assert_eq!(form.tabs, vec![tab("One", "first"), tab("Two", "second")]);
    }

    /// Repeated fields are exactly what axum's `Form` cannot handle, which is why this is parsed by
    /// hand — the same defect that broke installing a plugin on more than one server.
    #[test]
    fn several_tabs_all_survive() {
        let form = parse_federation_form(
            "tab_title=A&tab_html=1&tab_title=B&tab_html=2&tab_title=C&tab_html=3",
        );

        assert_eq!(form.tabs.len(), 3);
        assert_eq!(form.tabs[2], tab("C", "3"));
    }

    #[test]
    fn each_mode_round_trips() {
        for (submitted, expected) in [
            ("pinned", CustomTabsMode::Pinned),
            ("merged", CustomTabsMode::Merged),
            ("proxy", CustomTabsMode::Proxy),
        ] {
            let form = parse_federation_form(&format!("custom_tabs_mode={submitted}"));
            assert_eq!(form.mode, expected);
        }
    }

    /// A malformed or missing mode keeps the behaviour that existed before the setting, rather than
    /// silently switching an installation onto a policy nobody chose.
    #[test]
    fn an_unrecognised_mode_falls_back_to_pinned() {
        assert_eq!(
            parse_federation_form("custom_tabs_mode=nonsense").mode,
            CustomTabsMode::Pinned
        );
        assert_eq!(parse_federation_form("").mode, CustomTabsMode::Pinned);
    }

    /// Lists are typed one per line, but comma-separated is how they are written everywhere else,
    /// so accepting both avoids a setting that looks ignored.
    #[test]
    fn lists_accept_newlines_and_commas() {
        assert_eq!(
            parse_lines("/PluginPages\n/CustomTabs"),
            vec!["/PluginPages", "/CustomTabs"]
        );
        assert_eq!(
            parse_lines("/PluginPages, /CustomTabs"),
            vec!["/PluginPages", "/CustomTabs"]
        );
        assert_eq!(parse_lines("  \n\n /OnlyOne \n "), vec!["/OnlyOne"]);
        assert!(parse_lines("   ").is_empty());
    }

    /// The two route lists do different things and must not be conflated: assets are relayed
    /// verbatim, API routes go through normal processing so the token is remapped.
    #[test]
    fn the_two_route_lists_are_parsed_independently() {
        let form = parse_federation_form(
            "plugin_asset_prefixes=%2FMediaBar&plugin_api_prefixes=%2FJellyfinEnhanced%0A%2FJellyTweaks",
        );

        assert_eq!(form.plugin_asset_prefixes, vec!["/MediaBar"]);
        assert_eq!(
            form.plugin_api_prefixes,
            vec!["/JellyfinEnhanced", "/JellyTweaks"]
        );
    }

    /// The merged list carves exceptions out of the pinned one, so both arrive together and the
    /// longer entries must survive as their own list rather than folding into the prefix that pins
    /// them.
    #[test]
    fn merged_routes_are_parsed_separately_from_pinned_ones() {
        let form = parse_federation_form(
            "plugin_api_prefixes=%2FJellyfinEnhanced&federated_plugin_api_prefixes=%2FJellyfinEnhanced%2Ftag-cache%0A%2FJellyfinEnhanced%2Ftag-data",
        );

        assert_eq!(form.plugin_api_prefixes, vec!["/JellyfinEnhanced"]);
        assert_eq!(
            form.federated_plugin_api_prefixes,
            vec!["/JellyfinEnhanced/tag-cache", "/JellyfinEnhanced/tag-data"]
        );
    }

    /// A plugin's tag cache only covers the library of the server holding it, so pinning it leaves
    /// every item from every other server unbadged. Shipping the exception as a default spares an
    /// operator diagnosing a half-tagged library.
    #[test]
    fn enhanced_tag_routes_are_merged_by_default() {
        let defaults = crate::config::PluginFederationConfig::default();
        assert!(defaults
            .federated_plugin_api_prefixes
            .iter()
            .any(|prefix| prefix == "/JellyfinEnhanced/tag-cache"));
    }

    /// Jellyfin Enhanced identifies the caller from the access token, so an unpinned request
    /// reaches an arbitrary server and is rejected. Shipping it as a default means an operator does
    /// not have to diagnose that themselves.
    #[test]
    fn jellyfin_enhanced_is_pinned_by_default() {
        let defaults = crate::config::PluginFederationConfig::default();
        assert!(defaults
            .plugin_api_prefixes
            .iter()
            .any(|prefix| prefix == "/JellyfinEnhanced"));
    }

    /// Windows browsers submit CRLF in textareas; a stray `\r` on every entry would break the
    /// prefix matching these lists feed.
    #[test]
    fn carriage_returns_do_not_survive_into_entries() {
        assert_eq!(
            parse_lines("/PluginPages\r\n/CustomTabs\r\n"),
            vec!["/PluginPages", "/CustomTabs"]
        );
    }

    /// Tab bodies are HTML and must survive exactly as typed — trimming or unescaping them would
    /// quietly corrupt an iframe embed, which is the main thing people put here.
    #[test]
    fn tab_html_is_preserved_verbatim() {
        let form = parse_federation_form(
            "tab_title=Requests&tab_html=%3Ciframe+src%3D%22https%3A%2F%2Fexample.com%22%3E%3C%2Fiframe%3E",
        );

        assert_eq!(
            form.tabs[0].content_html,
            r#"<iframe src="https://example.com"></iframe>"#
        );
    }
}
