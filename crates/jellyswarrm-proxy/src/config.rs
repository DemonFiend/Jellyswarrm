use serde::{Deserialize, Serialize};
use serde_default::DefaultFromSerde;
use sqlx::migrate::Migrator;
use std::fmt;
use std::fs;
use std::io::Write;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::LazyLock;
use tower_sessions::cookie::Key;
use tracing::info;
use uuid::Uuid;

use jellyfin_api::ClientInfo;

use base64::prelude::*;

use crate::encryption::Password;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaStreamingMode {
    Redirect,
    Proxy,
}

impl std::str::FromStr for MediaStreamingMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "redirect" => Ok(MediaStreamingMode::Redirect),
            "proxy" => Ok(MediaStreamingMode::Proxy),
            _ => Err(format!("Invalid media streaming mode: {}", s)),
        }
    }
}

impl fmt::Display for MediaStreamingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaStreamingMode::Redirect => write!(f, "Redirect"),
            MediaStreamingMode::Proxy => write!(f, "Proxy"),
        }
    }
}

pub static MIGRATOR: Migrator = sqlx::migrate!();

pub static CLIENT_INFO: LazyLock<ClientInfo> = LazyLock::new(|| ClientInfo {
    client: "Jellyswarrm Proxy".to_string(),
    device: "Server".to_string(),
    device_id: "jellyswarrm-proxy".to_string(),
    version: env!("CARGO_PKG_VERSION").to_string(),
});

pub static CLIENT_STORAGE: LazyLock<jellyfin_api::storage::JellyfinClientStorage> =
    LazyLock::new(|| {
        jellyfin_api::storage::JellyfinClientStorage::new(
            300,
            std::time::Duration::from_secs(60 * 15),
        )
        // 15 minutes
    });

// Lazily-resolved data directory shared across the application.
// Priority: env var JELLYSWARRM_DATA_DIR, else "./data" relative to current working dir.
// The directory is created on first access.
pub static DATA_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    let base = std::env::var("JELLYSWARRM_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|path| path.join("data"))
                .unwrap_or_else(|e| {
                    eprintln!("Failed to resolve current directory, using ./data: {e}");
                    PathBuf::from("data")
                })
        });
    if let Err(e) = std::fs::create_dir_all(&base) {
        eprintln!("Failed to create data directory {base:?}: {e}");
    }
    base
});

fn default_server_id() -> String {
    Uuid::new_v4().simple().to_string()
}

fn default_public_address() -> String {
    "localhost:3000".to_string()
}

fn default_server_name() -> String {
    "Jellyswarrm Proxy".to_string()
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    3000
}

fn default_include_server_name_in_media() -> bool {
    true
}

fn default_username() -> String {
    "admin".to_string()
}

fn default_password() -> Password {
    "jellyswarrm".to_string().into()
}

fn default_session_key() -> Vec<u8> {
    Key::generate().master().to_vec()
}

fn default_timeout() -> u64 {
    20
}

/// Origin that serves the browser's Jellyfin web client, if the proxy should not serve its own.
///
/// Deliberately a URL rather than a reference to a configured server. The client and the plugin UI
/// it carries are a presentation concern, not a media concern: pointing this at a dedicated
/// plugin-host instance that holds no libraries is a config change rather than a rewrite.
///
/// Empty means "serve the bundled client", which is the previous behaviour.
fn default_web_client_host() -> String {
    String::new()
}

/// Per-server deadline for one leg of a federated fan-out, in seconds.
///
/// Deliberately shorter than [`default_timeout`]: a merged response is only as fast as its slowest
/// leg, so a single degraded upstream must not be able to hold the whole response for the full
/// client timeout. A leg that overruns is counted as a failure and the merge proceeds without it.
fn default_federated_leg_timeout() -> u64 {
    10
}

fn default_ui_route() -> UrlSegment {
    UrlSegment("ui".to_string())
}

fn default_media_streaming_mode() -> MediaStreamingMode {
    MediaStreamingMode::Proxy
}

fn default_server_background_check_interval_secs() -> u64 {
    30
}

fn default_auto_create_users_on_login() -> bool {
    true
}

fn default_merge_libraries() -> bool {
    true
}

mod base64_serde {
    use super::*;
    use serde::de::Error as DeError;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = BASE64_STANDARD.encode(bytes);
        serializer.serialize_str(&s)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        BASE64_STANDARD.decode(&s).map_err(D::Error::custom)
    }
}

macro_rules! define_fallback_deserializer {
    ($name:ident, $type:ty, $fallback_fn:path) => {
        fn $name<'de, D>(deserializer: D) -> Result<$type, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            use serde::Deserialize;
            let v: Result<serde_json::Value, _> = Deserialize::deserialize(deserializer);
            match v {
                Ok(val) => {
                    // First try direct deserialization (handles numbers, booleans, etc.)
                    if let Ok(t) = serde_json::from_value::<$type>(val.clone()) {
                        return Ok(t);
                    }

                    // If that fails, try parsing from string if it is a string
                    if let serde_json::Value::String(s) = &val {
                        match s.parse::<$type>() {
                            Ok(parsed) => return Ok(parsed),
                            Err(_) => tracing::info!(
                                "Ignoring invalid value for {}: '{}', falling back to default",
                                stringify!($name),
                                s
                            ),
                        }
                    } else {
                        tracing::info!(
                            "Ignoring invalid value for {}, falling back to default",
                            stringify!($name)
                        );
                    }

                    Ok($fallback_fn())
                }
                Err(_) => {
                    tracing::info!(
                        "Ignoring invalid configuration structure for {}, falling back to default",
                        stringify!($name)
                    );
                    Ok($fallback_fn())
                }
            }
        }
    };
}

define_fallback_deserializer!(deserialize_port, u16, default_port);
define_fallback_deserializer!(deserialize_host, String, default_host);
define_fallback_deserializer!(
    deserialize_include_server_name_in_media,
    bool,
    default_include_server_name_in_media
);
define_fallback_deserializer!(deserialize_timeout, u64, default_timeout);
define_fallback_deserializer!(deserialize_web_client_host, String, default_web_client_host);
define_fallback_deserializer!(
    deserialize_federated_leg_timeout,
    u64,
    default_federated_leg_timeout
);
define_fallback_deserializer!(deserialize_ui_route, UrlSegment, default_ui_route);
define_fallback_deserializer!(
    deserialize_media_streaming_mode,
    MediaStreamingMode,
    default_media_streaming_mode
);
define_fallback_deserializer!(
    deserialize_server_background_check_interval_secs,
    u64,
    default_server_background_check_interval_secs
);
define_fallback_deserializer!(
    deserialize_auto_create_users_on_login,
    bool,
    default_auto_create_users_on_login
);
define_fallback_deserializer!(deserialize_merge_libraries, bool, default_merge_libraries);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PreconfiguredServer {
    pub url: String,
    pub name: String,
    pub priority: i32,
    #[serde(default = "default_media_streaming_mode")]
    pub media_streaming_mode: MediaStreamingMode,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DebugUser {
    pub username: String,
    pub password: Password,
}

/// Where the tab list served at `/CustomTabs/config` comes from.
///
/// Custom Tabs stores its tabs per server, but a browser talks to one origin, so the proxy has to
/// decide whose tabs are real. The default preserves the behaviour this replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CustomTabsMode {
    /// Only the pinned client host's tabs. Tabs configured on any other server are invisible, with
    /// nothing logged — which is why this is worth being able to change.
    #[default]
    Pinned,
    /// Every server's tabs, concatenated and deduplicated by title.
    Merged,
    /// Only tabs defined on the proxy. Upstream tabs are ignored, so a tab can exist for people
    /// coming through the proxy without existing for anyone connecting to a server directly.
    Proxy,
}

/// A tab defined on the proxy rather than on a media server.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProxyCustomTab {
    pub title: String,
    #[serde(default)]
    pub content_html: String,
}

fn default_custom_tabs_mode() -> CustomTabsMode {
    CustomTabsMode::Pinned
}

fn default_custom_tabs() -> Vec<ProxyCustomTab> {
    Vec::new()
}

/// Top-level routes relayed to the pinned client host.
///
/// `/MediaBar` is kept even though the current Media Bar release loads its script from a CDN rather
/// than serving it — the plugin still exposes configuration endpoints under that prefix, and older
/// releases self-host the asset.
fn default_plugin_asset_prefixes() -> Vec<String> {
    [
        "/PluginPages",
        "/MediaBar",
        "/CustomTabs",
        "/HomeScreen/home-screen-sections.js",
        "/HomeScreen/home-screen-sections.css",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Plugin routes that must reach the server whose web client is being served, **authenticated**.
///
/// Distinct from [`default_plugin_asset_prefixes`] in two ways that matter. Those are relayed
/// verbatim, which is correct for scripts and stylesheets but sends the *proxy's* access token — so
/// any endpoint that identifies the caller sees an unknown user. And they only apply to paths with
/// a route registered for them, so adding a new prefix there has no effect. These go through normal
/// request processing instead: the token is swapped for the target server's and ids are remapped,
/// exactly as for any other API call, but the server is pinned rather than resolved per request.
///
/// The default covers Jellyfin Enhanced, whose endpoints identify the user from the token and are
/// otherwise answered by whichever server a request happens to resolve to — a server that may not
/// run the plugin at all.
fn default_plugin_api_prefixes() -> Vec<String> {
    ["/JellyfinEnhanced", "/JellyTweaks"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Plugin routes answering about *items* rather than about the server or the user.
///
/// Pinning sends every plugin call to the one server that injected the script, which is right for
/// anything describing that plugin or that user — its version, its settings — and wrong for
/// anything describing the library, because each server only knows its own. Jellyfin Enhanced's
/// tag cache is the case in point: pinned, half a merged library gets no quality or rating badge,
/// since the pinned server has never heard of the other server's items.
///
/// A prefix listed here is asked of every server the user has a session on and the answers merged.
/// Being a subset of [`default_plugin_api_prefixes`], the entries must be longer than the prefix
/// that pins them, so ordering matters to whoever edits the list: federation is checked first.
fn default_federated_plugin_api_prefixes() -> Vec<String> {
    ["/JellyfinEnhanced/tag-cache", "/JellyfinEnhanced/tag-data"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Home screen sections whose content comes from an external service rather than the local library,
/// matched as normalised prefixes because the plugin names variants by suffix.
fn default_single_source_section_prefixes() -> Vec<String> {
    ["discover", "myjellyseerrrequests", "jellyseerr", "upcoming"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Sections that list the user's libraries rather than media items.
fn default_library_view_sections() -> Vec<String> {
    vec!["mymedia".to_string()]
}

/// Per-plugin federation policy.
///
/// Client-side plugins keep their configuration on each media server, so every one of them raises
/// the same question: when several servers disagree, whose answer does the browser get? These were
/// previously four separate hardcoded lists in three modules, which meant answering that question
/// required a rebuild and gave no way to tell what the current policy was.
#[derive(Debug, Clone, Deserialize, Serialize, DefaultFromSerde)]
pub struct PluginFederationConfig {
    #[serde(default = "default_custom_tabs_mode")]
    pub custom_tabs_mode: CustomTabsMode,

    /// Tabs owned by the proxy, used by [`CustomTabsMode::Proxy`] and appended by
    /// [`CustomTabsMode::Merged`].
    #[serde(default = "default_custom_tabs")]
    pub custom_tabs: Vec<ProxyCustomTab>,

    #[serde(default = "default_plugin_asset_prefixes")]
    pub plugin_asset_prefixes: Vec<String>,

    #[serde(default = "default_plugin_api_prefixes")]
    pub plugin_api_prefixes: Vec<String>,

    #[serde(default = "default_federated_plugin_api_prefixes")]
    pub federated_plugin_api_prefixes: Vec<String>,

    #[serde(default = "default_single_source_section_prefixes")]
    pub single_source_section_prefixes: Vec<String>,

    #[serde(default = "default_library_view_sections")]
    pub library_view_sections: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize, DefaultFromSerde)]
pub struct AppConfig {
    #[serde(default = "default_server_id")]
    pub server_id: String,
    #[serde(default = "default_public_address")]
    pub public_address: String,
    #[serde(default = "default_server_name")]
    pub server_name: String,
    #[serde(default = "default_host", deserialize_with = "deserialize_host")]
    pub host: String,
    #[serde(default = "default_port", deserialize_with = "deserialize_port")]
    pub port: u16,
    #[serde(
        default = "default_include_server_name_in_media",
        deserialize_with = "deserialize_include_server_name_in_media"
    )]
    pub include_server_name_in_media: bool,

    #[serde(default = "default_username")]
    pub username: String,
    #[serde(default = "default_password")]
    pub password: Password,

    #[serde(default)]
    pub preconfigured_servers: Vec<PreconfiguredServer>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_user: Option<DebugUser>,

    #[serde(default = "default_session_key", with = "base64_serde")]
    pub session_key: Vec<u8>,

    #[serde(default = "default_timeout", deserialize_with = "deserialize_timeout")]
    pub timeout: u64, // in seconds

    #[serde(
        default = "default_federated_leg_timeout",
        deserialize_with = "deserialize_federated_leg_timeout"
    )]
    pub federated_leg_timeout: u64, // in seconds

    #[serde(
        default = "default_web_client_host",
        deserialize_with = "deserialize_web_client_host"
    )]
    pub web_client_host: String,

    #[serde(
        default = "default_ui_route",
        deserialize_with = "deserialize_ui_route"
    )]
    pub ui_route: UrlSegment,

    #[serde(default)]
    pub url_prefix: Option<UrlSegment>,

    #[serde(
        default = "default_media_streaming_mode",
        deserialize_with = "deserialize_media_streaming_mode"
    )]
    pub media_streaming_mode: MediaStreamingMode,

    #[serde(
        default = "default_server_background_check_interval_secs",
        deserialize_with = "deserialize_server_background_check_interval_secs"
    )]
    pub server_background_check_interval_secs: u64,

    #[serde(
        default = "default_auto_create_users_on_login",
        deserialize_with = "deserialize_auto_create_users_on_login"
    )]
    pub auto_create_users_on_login: bool,

    #[serde(
        default = "default_merge_libraries",
        deserialize_with = "deserialize_merge_libraries"
    )]
    pub merge_libraries: bool,

    #[serde(default)]
    pub plugin_federation: PluginFederationConfig,
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let session_key = format!("<{} bytes>", self.session_key.len());
        f.debug_struct("AppConfig")
            .field("server_id", &self.server_id)
            .field("public_address", &self.public_address)
            .field("server_name", &self.server_name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field(
                "include_server_name_in_media",
                &self.include_server_name_in_media,
            )
            .field("username", &self.username)
            .field("password", &self.password)
            .field("preconfigured_servers", &self.preconfigured_servers)
            .field("debug_user", &self.debug_user)
            .field("session_key", &session_key)
            .field("timeout", &self.timeout)
            .field("federated_leg_timeout", &self.federated_leg_timeout)
            .field("web_client_host", &self.web_client_host)
            .field("ui_route", &self.ui_route)
            .field("url_prefix", &self.url_prefix)
            .field("media_streaming_mode", &self.media_streaming_mode)
            .field(
                "server_background_check_interval_secs",
                &self.server_background_check_interval_secs,
            )
            .field(
                "auto_create_users_on_login",
                &self.auto_create_users_on_login,
            )
            .finish()
    }
}

pub const DEFAULT_CONFIG_FILENAME: &str = "jellyswarrm.toml";

fn config_path() -> PathBuf {
    DATA_DIR.join(DEFAULT_CONFIG_FILENAME)
}

#[allow(dead_code)]
fn dev_config_path() -> PathBuf {
    const DEV_CONFIG_FILENAME: &str = "jellyswarrm.dev.toml";
    DATA_DIR.join(DEV_CONFIG_FILENAME)
}

/// Load configuration from known files and environment. Falls back to defaults.
pub fn load_config() -> AppConfig {
    let path = config_path();
    let builder = if cfg!(debug_assertions) {
        // In debug mode, also load a dev-specific config file if it exists.
        info!(
            "Loading config from {path:?} and dev config from {dev_config_path:?}",
            dev_config_path = dev_config_path()
        );
        config::Config::builder()
            .add_source(config::File::with_name(path.to_string_lossy().as_ref()).required(false))
            .add_source(
                config::File::with_name(dev_config_path().to_string_lossy().as_ref())
                    .required(false),
            )
            .add_source(config::Environment::with_prefix("JELLYSWARRM").separator("_"))
    } else {
        config::Config::builder()
            .add_source(config::File::with_name(path.to_string_lossy().as_ref()).required(false))
            .add_source(config::Environment::with_prefix("JELLYSWARRM").separator("_"))
    };

    // A config file that exists but cannot be read must never fall back to defaults. The defaults
    // include the admin password published in the README, a freshly generated `server_id` (which
    // every saved client keys on, so changing it strands them all) and a new `session_key` (which
    // invalidates every session). Coming up silently in that state is worse than not coming up:
    // per-field problems are already absorbed by the fallback deserializers, so reaching here means
    // the file is structurally broken and a human needs to look at it.
    let config_file_exists = path.exists();
    let config = match builder.build() {
        Ok(c) => match c.try_deserialize::<AppConfig>() {
            Ok(config) => config,
            Err(e) if config_file_exists => {
                eprintln!(
                    "Refusing to start: {path:?} exists but could not be parsed: {e}
                     Starting with defaults here would reset the admin credentials, the server id                      and the session key. Fix or remove the file and start again."
                );
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("No usable config found, starting with defaults: {e}");
                AppConfig::default()
            }
        },
        Err(e) if config_file_exists => {
            eprintln!(
                "Refusing to start: {path:?} exists but could not be loaded: {e}
                 Starting with defaults here would reset the admin credentials, the server id and                  the session key. Fix or remove the file and start again."
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("No usable config found, starting with defaults: {e}");
            AppConfig::default()
        }
    };

    if !path.exists() {
        if let Err(e) = save_config(&config) {
            eprintln!("Failed to save default config to {path:?}: {e}");
        }
    }

    config
}

/// Persist configuration to the first existing file or the primary default file.
pub fn save_config(cfg: &AppConfig) -> std::io::Result<()> {
    let toml_str = toml::to_string_pretty(cfg).map_err(std::io::Error::other)?;
    let path = config_path();
    let temp_path = path.with_extension(format!("toml.tmp-{}", std::process::id()));
    let write_result = (|| {
        let mut file = fs::File::create(&temp_path)?;
        file.write_all(toml_str.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp_path, &path)
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(temp_path);
        return Err(error);
    }
    info!("Configuration saved to {path:?}");
    Ok(())
}

// A normalized URL path segment (no leading/trailing slashes, non-empty).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UrlSegment(String);

impl UrlSegment {
    pub fn new<S: Into<String>>(s: S) -> Result<Self, &'static str> {
        let t = s
            .into()
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string();
        if t.is_empty() {
            Err("empty UrlSegment")
        } else {
            Ok(UrlSegment(t))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for UrlSegment {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for UrlSegment {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UrlSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for UrlSegment {
    fn from(s: String) -> Self {
        // best-effort: create without returning error (used for programmatic conversions)
        UrlSegment(s.trim_start_matches('/').trim_end_matches('/').to_string())
    }
}

impl From<&str> for UrlSegment {
    fn from(s: &str) -> Self {
        UrlSegment::from(s.to_string())
    }
}

impl std::str::FromStr for UrlSegment {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        UrlSegment::new(s)
    }
}

impl serde::Serialize for UrlSegment {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for UrlSegment {
    fn deserialize<D>(deserializer: D) -> Result<UrlSegment, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let t = s.trim_start_matches('/').trim_end_matches('/').to_string();
        if t.is_empty() {
            Err(serde::de::Error::custom("url segment must not be empty"))
        } else {
            Ok(UrlSegment(t))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-leg deadline only has an effect while it is strictly tighter than the shared reqwest
    /// client timeout. If the two ever converge, a slow upstream would again be able to hold a
    /// merged response for the full client timeout and this guard would silently stop guarding.
    #[test]
    fn federated_leg_timeout_is_tighter_than_the_global_client_timeout() {
        let config = AppConfig::default();

        assert!(
            config.federated_leg_timeout < config.timeout,
            "per-leg timeout ({}s) must stay below the global client timeout ({}s)",
            config.federated_leg_timeout,
            config.timeout
        );
        assert!(
            config.federated_leg_timeout > 0,
            "a zero per-leg timeout would fail every federated request"
        );
    }

    /// The defaults are not a safe fallback for a config that failed to parse: they carry the admin
    /// password published in the README, a freshly generated `server_id` — which every saved client
    /// keys on — and a new `session_key`. This pins the properties that make silently defaulting
    /// unacceptable, so the guard in `load_config` cannot be relaxed without this failing.
    #[test]
    fn defaults_are_not_a_safe_fallback_for_a_broken_config() {
        let first = AppConfig::default();
        let second = AppConfig::default();

        assert_eq!(
            first.password.as_str(),
            "jellyswarrm",
            "the default password is the published one, so defaulting exposes it"
        );
        assert_ne!(
            first.server_id, second.server_id,
            "server_id is regenerated per default, which would strand every saved client"
        );
        assert_ne!(
            first.session_key, second.session_key,
            "session_key is regenerated per default, which would invalidate every session"
        );
    }
}
