use serde_json::Value;
use sqlx::SqlitePool;

/// Sentinel "user" under which the admin-defined default Home layout is stored.
/// A real virtual user id is a hex token, so this never collides with one.
pub const DEFAULT_USER_ID: &str = "__default__";
/// Sentinel client used for the default DisplayPreferences row (the default is
/// client-agnostic — every device inherits the same layout).
pub const DEFAULT_CLIENT: &str = "__default__";
/// The DisplayPreferences id the Jellyfin web/JMP client uses for home layout.
pub const USERSETTINGS_PREFS_ID: &str = "usersettings";

/// Stores Home-screen customization on the proxy itself, keyed by the virtual
/// user id, so it persists regardless of which upstream server answers a given
/// request (see upstream issue #23). Two kinds of state:
///
/// * `display_preferences` — the raw Jellyfin DisplayPreferences object per
///   `(user, client, prefs_id)` (carries `CustomPrefs` / home sections).
/// * `user_configuration` — the user's `UserConfiguration` blob (carries
///   `OrderedViews`, the library order).
///
/// All IDs persisted here are Jellyswarrm *virtual* IDs, identical to what the
/// client already sees, so nothing is ever remapped or forwarded upstream.
#[derive(Debug, Clone)]
pub struct DisplayPreferencesService {
    pool: SqlitePool,
}

impl DisplayPreferencesService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn get_display_preferences(
        &self,
        user_id: &str,
        client: &str,
        prefs_id: &str,
    ) -> Result<Option<Value>, sqlx::Error> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT data FROM display_preferences \
             WHERE user_id = ? AND client = ? AND prefs_id = ?",
        )
        .bind(user_id)
        .bind(client)
        .bind(prefs_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.and_then(|(data,)| serde_json::from_str(&data).ok()))
    }

    pub async fn set_display_preferences(
        &self,
        user_id: &str,
        client: &str,
        prefs_id: &str,
        data: &Value,
    ) -> Result<(), sqlx::Error> {
        let serialized = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
        sqlx::query(
            "INSERT INTO display_preferences (user_id, client, prefs_id, data, updated_at) \
             VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(user_id, client, prefs_id) \
             DO UPDATE SET data = excluded.data, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(user_id)
        .bind(client)
        .bind(prefs_id)
        .bind(&serialized)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_user_configuration(
        &self,
        user_id: &str,
    ) -> Result<Option<Value>, sqlx::Error> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data FROM user_configuration WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.and_then(|(data,)| serde_json::from_str(&data).ok()))
    }

    pub async fn set_user_configuration(
        &self,
        user_id: &str,
        data: &Value,
    ) -> Result<(), sqlx::Error> {
        let serialized = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
        sqlx::query(
            "INSERT INTO user_configuration (user_id, data, updated_at) \
             VALUES (?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(user_id) \
             DO UPDATE SET data = excluded.data, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(user_id)
        .bind(&serialized)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The user's saved library order (`OrderedViews`, virtual library IDs).
    /// Falls back to the admin-defined default layout when the user hasn't set one.
    pub async fn get_ordered_views(
        &self,
        user_id: &str,
    ) -> Result<Option<Vec<String>>, sqlx::Error> {
        if let Some(cfg) = self.get_user_configuration(user_id).await? {
            if let Some(views) = extract_ordered_views(&cfg) {
                if !views.is_empty() {
                    return Ok(Some(views));
                }
            }
        }
        // Fall back to the admin default (but not when we ARE the default row).
        if user_id != DEFAULT_USER_ID {
            if let Some(cfg) = self.get_user_configuration(DEFAULT_USER_ID).await? {
                return Ok(extract_ordered_views(&cfg).filter(|v| !v.is_empty()));
            }
        }
        Ok(None)
    }

    /// The admin default DisplayPreferences (home sections), if one is set.
    pub async fn get_default_display_preferences(&self) -> Result<Option<Value>, sqlx::Error> {
        self.get_display_preferences(DEFAULT_USER_ID, DEFAULT_CLIENT, USERSETTINGS_PREFS_ID)
            .await
    }

    /// Copy a user's current Home layout (home sections + library order) into the
    /// global default that un-customized users inherit on every device.
    pub async fn promote_user_to_default(&self, user_id: &str) -> Result<bool, sqlx::Error> {
        let prefs = self
            .get_display_preferences(user_id, "emby", USERSETTINGS_PREFS_ID)
            .await?;
        let cfg = self.get_user_configuration(user_id).await?;

        // Nothing to copy if the user has never customized their home.
        if prefs.is_none() && cfg.is_none() {
            return Ok(false);
        }
        if let Some(prefs) = prefs {
            self.set_display_preferences(
                DEFAULT_USER_ID,
                DEFAULT_CLIENT,
                USERSETTINGS_PREFS_ID,
                &prefs,
            )
            .await?;
        }
        if let Some(cfg) = cfg {
            self.set_user_configuration(DEFAULT_USER_ID, &cfg).await?;
        }
        Ok(true)
    }

    /// Whether an admin default layout is currently set.
    pub async fn has_default(&self) -> Result<bool, sqlx::Error> {
        Ok(self.get_default_display_preferences().await?.is_some()
            || self.get_user_configuration(DEFAULT_USER_ID).await?.is_some())
    }

    /// Remove the global default layout.
    pub async fn clear_default(&self) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM display_preferences WHERE user_id = ?")
            .bind(DEFAULT_USER_ID)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM user_configuration WHERE user_id = ?")
            .bind(DEFAULT_USER_ID)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// Extract `OrderedViews` (virtual library IDs) from a UserConfiguration JSON blob.
fn extract_ordered_views(cfg: &Value) -> Option<Vec<String>> {
    cfg.get("OrderedViews")
        .or_else(|| cfg.get("orderedViews"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
}
