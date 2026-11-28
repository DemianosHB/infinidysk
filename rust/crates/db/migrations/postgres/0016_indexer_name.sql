-- v1-fork indexer statistics: the indexer a submission came from (matched by NZB URL
-- host, or named by the play/search path), carried from the queue into the history so
-- the overview can score indexers. NULL for submissions without a known indexer.
ALTER TABLE queue ADD COLUMN indexer_name TEXT;
ALTER TABLE history ADD COLUMN indexer_name TEXT;
CREATE INDEX history_indexer ON history (indexer_name, created_at DESC);
