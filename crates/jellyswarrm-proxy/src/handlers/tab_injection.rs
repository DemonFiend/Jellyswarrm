//! Rendering custom tabs from the proxy, without depending on the Custom Tabs plugin.
//!
//! The plugin renders its tabs by hooking jellyfin-web's router. That hook broke in 10.11 — clicking
//! a tab makes the router attempt a dynamic import that resolves to `'./'`, which throws, and the
//! tab's content is never mounted. The visible result is a blank page with no error, because the
//! author's own fallback markup is part of the content that never rendered.
//!
//! The proxy is already in the response path for `/web/*` and already decides the tab list, so it
//! can render them itself. Doing so removes the dependency entirely: tabs work whether or not the
//! plugin is installed, and on Jellyfin versions the plugin does not target.
//!
//! Two decisions keep this from becoming a maintenance problem:
//!
//! * **No router involvement.** Tabs open an overlay rather than registering a route, so the
//!   failure mode above is not merely fixed but unrepresentable. A routed tab would look more
//!   native and be considerably more fragile.
//! * **Fail silent, never throw.** Injected code runs inside someone else's application. If the
//!   header is not where the script expects, it does nothing — a missing button is a far better
//!   outcome than an exception that takes the page down with it.

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::Response,
};

use crate::AppState;

/// Path the injected script is served from.
///
/// Namespaced under `/jellyswarrm` so it cannot collide with a plugin route, present or future.
pub const TAB_SCRIPT_PATH: &str = "/jellyswarrm/custom-tabs.js";

/// Marker used to keep injection idempotent.
const INJECTION_MARKER: &str = "data-jellyswarrm-tabs";

/// The injected client script.
// `r##"` rather than `r#"`: the script contains `"#" + …` for CSS id selectors, which would end a
// single-hash raw string partway through.
const TAB_SCRIPT: &str = r##"/* Jellyswarrm custom tabs — rendered by the proxy, independent of the Custom Tabs plugin. */
(function () {
  "use strict";

  var OVERLAY_ID = "jellyswarrm-tab-overlay";
  var BUTTON_CLASS = "jellyswarrm-tab-button";

  function styles() {
    if (document.getElementById("jellyswarrm-tab-styles")) return;
    var css = document.createElement("style");
    css.id = "jellyswarrm-tab-styles";
    css.textContent =
      "." + BUTTON_CLASS + "{background:none;border:0;color:inherit;font:inherit;cursor:pointer;padding:0 .75em;opacity:.8}" +
      "." + BUTTON_CLASS + ":hover{opacity:1}" +
      "#" + OVERLAY_ID + "{position:fixed;inset:0;z-index:9999;background:#101010;display:flex;flex-direction:column}" +
      "#" + OVERLAY_ID + " .jellyswarrm-tab-bar{display:flex;align-items:center;gap:.5em;padding:.4em .8em;background:#181818;color:#fff;font:14px/1.4 sans-serif;flex:0 0 auto}" +
      "#" + OVERLAY_ID + " .jellyswarrm-tab-bar button{margin-left:auto;background:none;border:1px solid #555;color:#fff;border-radius:4px;padding:.2em .7em;cursor:pointer}" +
      "#" + OVERLAY_ID + " .jellyswarrm-tab-body{flex:1 1 auto;overflow:auto;position:relative}";
    document.head.appendChild(css);
  }

  function close() {
    var existing = document.getElementById(OVERLAY_ID);
    if (existing) existing.remove();
  }

  /* innerHTML does not execute <script>, but the tab body is admin-authored HTML that may rely on
     it — the documented iframe-fallback pattern does. Re-create each script so it runs. */
  function runScripts(container) {
    var scripts = container.querySelectorAll("script");
    for (var i = 0; i < scripts.length; i++) {
      var original = scripts[i];
      var replacement = document.createElement("script");
      for (var a = 0; a < original.attributes.length; a++) {
        replacement.setAttribute(original.attributes[a].name, original.attributes[a].value);
      }
      replacement.text = original.text;
      original.parentNode.replaceChild(replacement, original);
    }
  }

  function open(tab) {
    close();
    styles();

    var overlay = document.createElement("div");
    overlay.id = OVERLAY_ID;

    var bar = document.createElement("div");
    bar.className = "jellyswarrm-tab-bar";
    var title = document.createElement("span");
    title.textContent = tab.Title || "";
    var closeButton = document.createElement("button");
    closeButton.type = "button";
    closeButton.textContent = "Close";
    closeButton.addEventListener("click", close);
    bar.appendChild(title);
    bar.appendChild(closeButton);

    var body = document.createElement("div");
    body.className = "jellyswarrm-tab-body";
    body.innerHTML = tab.ContentHtml || "";

    overlay.appendChild(bar);
    overlay.appendChild(body);
    document.body.appendChild(overlay);
    runScripts(body);
  }

  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape") close();
  });

  /* jellyfin-web has moved its header markup between releases, so try the known containers in turn
     and give up quietly rather than guessing at the DOM. */
  function header() {
    var selectors = [".headerRight", ".skinHeader .headerRight", ".headerTabs", ".skinHeader"];
    for (var i = 0; i < selectors.length; i++) {
      var found = document.querySelector(selectors[i]);
      if (found) return found;
    }
    return null;
  }

  function render(tabs) {
    var target = header();
    if (!target || !tabs.length) return;

    var existing = target.querySelectorAll("." + BUTTON_CLASS);
    for (var i = 0; i < existing.length; i++) existing[i].remove();

    tabs.forEach(function (tab) {
      if (!tab || !tab.Title) return;
      var button = document.createElement("button");
      button.type = "button";
      button.className = BUTTON_CLASS;
      button.textContent = tab.Title;
      button.addEventListener("click", function () { open(tab); });
      target.insertBefore(button, target.firstChild);
    });
  }

  var tabs = null;

  function load() {
    return fetch("/CustomTabs/config", { credentials: "same-origin" })
      .then(function (response) { return response.ok ? response.json() : []; })
      .then(function (list) { tabs = Array.isArray(list) ? list : []; })
      .catch(function () { tabs = []; });
  }

  /* The header is rebuilt on navigation, so the buttons have to be re-added. An interval is used
     rather than a MutationObserver because it cannot wedge the page if the DOM churns. */
  function start() {
    load().then(function () {
      render(tabs);
      setInterval(function () { if (tabs) render(tabs); }, 2000);
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start);
  } else {
    start();
  }
})();
"##;

/// Appends the script tag to a web client document.
///
/// Placed before `</body>` so the application's own scripts are already present, and marked so a
/// document that somehow passes through twice is not injected twice.
pub fn inject_tab_script(html: &str) -> String {
    if html.contains(INJECTION_MARKER) {
        return html.to_string();
    }

    let tag = format!(r#"<script {INJECTION_MARKER} defer src="{TAB_SCRIPT_PATH}"></script>"#);

    for closing in ["</body>", "</BODY>"] {
        if let Some(index) = html.rfind(closing) {
            let mut out = String::with_capacity(html.len() + tag.len());
            out.push_str(&html[..index]);
            out.push_str(&tag);
            out.push_str(&html[index..]);
            return out;
        }
    }

    // No recognisable body: appending still runs, and is better than silently dropping the tabs.
    format!("{html}{tag}")
}

/// Whether a relayed response is a document worth injecting into.
///
/// Only full HTML documents qualify — injecting a script tag into a JSON or JavaScript response
/// would corrupt it.
pub fn is_html_document(content_type: Option<&str>) -> bool {
    content_type
        .map(|value| value.to_ascii_lowercase().starts_with("text/html"))
        .unwrap_or(false)
}

/// `GET /jellyswarrm/custom-tabs.js`
pub async fn tab_script(State(_state): State<AppState>) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/javascript; charset=utf-8")
        // Short-lived: the tab list is read at runtime, but the script itself changes with upgrades.
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(TAB_SCRIPT))
        .unwrap_or_else(|_| Response::new(Body::from(TAB_SCRIPT)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_is_added_before_the_closing_body_tag() {
        let injected = inject_tab_script("<html><body><div>app</div></body></html>");

        assert!(injected.contains(TAB_SCRIPT_PATH));
        let script_at = injected.find(TAB_SCRIPT_PATH).unwrap();
        let body_at = injected.find("</body>").unwrap();
        assert!(
            script_at < body_at,
            "the tag belongs inside the body, after the application's own scripts"
        );
    }

    /// A document that passes through twice must not get two buttons per tab.
    #[test]
    fn injecting_twice_changes_nothing_the_second_time() {
        let once = inject_tab_script("<html><body></body></html>");
        let twice = inject_tab_script(&once);

        assert_eq!(once, twice);
        assert_eq!(twice.matches(TAB_SCRIPT_PATH).count(), 1);
    }

    /// jellyfin-web's index is lowercase, but a relayed document is not guaranteed to be.
    #[test]
    fn an_uppercase_body_tag_is_still_found() {
        let injected = inject_tab_script("<HTML><BODY></BODY></HTML>");
        assert!(injected.contains(TAB_SCRIPT_PATH));
        assert!(injected.find(TAB_SCRIPT_PATH).unwrap() < injected.find("</BODY>").unwrap());
    }

    /// Losing the tabs entirely is worse than appending to an unusual document.
    #[test]
    fn a_document_without_a_body_still_gets_the_script() {
        let injected = inject_tab_script("<div>fragment</div>");
        assert!(injected.contains(TAB_SCRIPT_PATH));
        assert!(injected.starts_with("<div>fragment</div>"));
    }

    /// Only documents. Injecting into a bundle or an API response would corrupt it — and `/web/*`
    /// relays far more JavaScript and JSON than HTML.
    #[test]
    fn only_html_documents_are_injected_into() {
        assert!(is_html_document(Some("text/html")));
        assert!(is_html_document(Some("text/html; charset=utf-8")));
        assert!(is_html_document(Some("TEXT/HTML")));

        assert!(!is_html_document(Some("application/javascript")));
        assert!(!is_html_document(Some("application/json")));
        assert!(!is_html_document(Some("text/css")));
        assert!(!is_html_document(None));
    }

    /// The failure this exists to remove: the plugin hooks jellyfin-web's router, and in 10.11 that
    /// hook resolves a dynamic import to `'./'` and throws. Nothing here registers a route.
    #[test]
    fn the_script_never_touches_the_router() {
        assert!(!TAB_SCRIPT.contains("import("));
        assert!(!TAB_SCRIPT.contains("Emby.Page"));
        assert!(!TAB_SCRIPT.contains("appRouter"));
    }

    /// Admin-authored tab bodies commonly carry a script — the documented iframe fallback does —
    /// and `innerHTML` alone would never run it.
    #[test]
    fn embedded_scripts_are_re_executed() {
        assert!(TAB_SCRIPT.contains("runScripts"));
        assert!(TAB_SCRIPT.contains("createElement(\"script\")"));
    }
}
