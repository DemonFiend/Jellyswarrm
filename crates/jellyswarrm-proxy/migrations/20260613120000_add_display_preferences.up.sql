-- Proxy-owned Home customization storage (fixes upstream issue #23).
-- Jellyswarrm serves DisplayPreferences and the user's library order itself,
-- keyed by the virtual user id, instead of proxying them to whichever upstream
-- happens to answer (which made settings appear to "not save"). All IDs stored
-- here are virtual IDs, so nothing needs remapping and nothing is synced upstream.

CREATE TABLE IF NOT EXISTS display_preferences (
    user_id    TEXT NOT NULL,
    client     TEXT NOT NULL,
    prefs_id   TEXT NOT NULL,
    data       TEXT NOT NULL,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (user_id, client, prefs_id)
);

CREATE TABLE IF NOT EXISTS user_configuration (
    user_id    TEXT PRIMARY KEY NOT NULL,
    data       TEXT NOT NULL,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
