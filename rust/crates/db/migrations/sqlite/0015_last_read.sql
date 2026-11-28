-- Durable last-read time of external (WebDAV/FUSE) reads, in Unix milliseconds; NULL
-- until the first one. The unread-download cleanup counts it, the read path writes it at
-- most once a day per item.
ALTER TABLE items ADD COLUMN last_read_ms BIGINT;
