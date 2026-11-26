-- v1-fork Phase 3 (Watchtower): list sources and wanted items.
CREATE TABLE IF NOT EXISTS list_sources (
    id UUID PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    url TEXT,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    cap INTEGER NOT NULL DEFAULT 0,
    series_scope TEXT,
    created_at BIGINT NOT NULL DEFAULT 0,
    last_synced_at BIGINT,
    last_sync_error TEXT
);
CREATE INDEX IF NOT EXISTS ix_list_sources_kind ON list_sources(kind);

CREATE TABLE IF NOT EXISTS wanted_items (
    id UUID PRIMARY KEY NOT NULL,
    key TEXT NOT NULL,
    type TEXT NOT NULL,
    content_id TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'scouting',
    provenance TEXT NOT NULL DEFAULT '[]',
    shortlist TEXT NOT NULL DEFAULT '[]',
    winner_nzb BYTEA,
    responder_host TEXT,
    fail_reason TEXT,
    created_at BIGINT NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL DEFAULT 0,
    last_resolved_at BIGINT,
    last_verified_at BIGINT,
    next_check_at BIGINT
);
CREATE UNIQUE INDEX IF NOT EXISTS ix_wanted_items_key ON wanted_items(key);
CREATE INDEX IF NOT EXISTS ix_wanted_items_next_check ON wanted_items(next_check_at);
CREATE INDEX IF NOT EXISTS ix_wanted_items_state ON wanted_items(state);
