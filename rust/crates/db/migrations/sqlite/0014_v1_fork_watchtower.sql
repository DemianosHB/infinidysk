-- v1-fork Phase 3 (Watchtower): list sources and wanted items.
-- Ported from backend/Database/Migrations/20260602000000_Add-Watchtower-Tables.cs.
CREATE TABLE IF NOT EXISTS list_sources (
    id BLOB PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    url TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    cap INTEGER NOT NULL DEFAULT 0,
    series_scope TEXT,
    created_at INTEGER NOT NULL DEFAULT 0,
    last_synced_at INTEGER,
    last_sync_error TEXT
);
CREATE INDEX IF NOT EXISTS ix_list_sources_kind ON list_sources(kind);

CREATE TABLE IF NOT EXISTS wanted_items (
    id BLOB PRIMARY KEY NOT NULL,
    key TEXT NOT NULL,
    type TEXT NOT NULL,
    content_id TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'scouting',
    provenance TEXT NOT NULL DEFAULT '[]',
    shortlist TEXT NOT NULL DEFAULT '[]',
    winner_nzb BLOB,
    responder_host TEXT,
    fail_reason TEXT,
    created_at INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT 0,
    last_resolved_at INTEGER,
    last_verified_at INTEGER,
    next_check_at INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS ix_wanted_items_key ON wanted_items(key);
CREATE INDEX IF NOT EXISTS ix_wanted_items_next_check ON wanted_items(next_check_at);
CREATE INDEX IF NOT EXISTS ix_wanted_items_state ON wanted_items(state);
