-- The stored size of a build's artifact. See the SQLite variant for why it
-- lives on this table rather than being read from a folder listing.
ALTER TABLE impresspress__dev__builds
    ADD COLUMN IF NOT EXISTS artifact_bytes BIGINT NOT NULL DEFAULT 0;
