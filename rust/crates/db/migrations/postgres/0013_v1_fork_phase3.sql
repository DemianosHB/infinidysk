-- v1-fork Phase 3 tables: Warden dead-release list and play Watchdog entries.
CREATE TABLE IF NOT EXISTS warden_sources (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    url TEXT,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    trust TEXT NOT NULL DEFAULT 'full',
    refresh_hours INTEGER NOT NULL DEFAULT 24,
    last_checked BIGINT NOT NULL DEFAULT 0,
    last_updated BIGINT NOT NULL DEFAULT 0,
    etag TEXT,
    status TEXT,
    sort INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS warden_entries (
    source_id TEXT NOT NULL DEFAULT 'local',
    fp TEXT NOT NULL,
    dead_at BIGINT NOT NULL,
    n INTEGER NOT NULL,
    backbones TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (source_id, fp)
);
-- The 'local' source row is seeded at runtime (Db::warden_ensure_local_source),
-- not here: a freshly migrated database must stay row-free for the transfer check.
CREATE INDEX IF NOT EXISTS ix_warden_fp ON warden_entries(fp);
CREATE INDEX IF NOT EXISTS ix_warden_dead_at ON warden_entries(dead_at);

CREATE TABLE IF NOT EXISTS watchdog_entries (
    id BIGSERIAL PRIMARY KEY,
    click_id TEXT NOT NULL,
    attempted_at BIGINT NOT NULL,
    content_type TEXT NOT NULL,
    requested_title TEXT NOT NULL,
    candidate_title TEXT NOT NULL,
    indexer_name TEXT NOT NULL,
    size BIGINT NOT NULL,
    rank_index INTEGER NOT NULL,
    result INTEGER NOT NULL,
    fail_reason TEXT,
    duration_ms INTEGER NOT NULL,
    is_winner BOOLEAN NOT NULL DEFAULT FALSE,
    provider_host TEXT,
    queue_item_id TEXT,
    content_group_key TEXT
);
CREATE INDEX IF NOT EXISTS ix_watchdog_attempted ON watchdog_entries(attempted_at);
CREATE INDEX IF NOT EXISTS ix_watchdog_click ON watchdog_entries(click_id);
