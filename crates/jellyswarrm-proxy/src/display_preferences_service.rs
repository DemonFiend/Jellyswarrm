use serde_json::Value;
use sqlx::SqlitePool;

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

    /// The user's saved library order (`OrderedViews`, virtual library IDs), if any.
    pub async fn get_ordered_views(
        &self,
        user_id: &str,
    ) -> Result<Option<Vec<String>>, sqlx::Error> {
        let cfg = self.get_user_configuration(user_id).await?;
        Ok(cfg.and_then(|c| {
            c.get("OrderedViews")
                .or_else(|| c.get("orderedViews"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
        }))
    }
}
