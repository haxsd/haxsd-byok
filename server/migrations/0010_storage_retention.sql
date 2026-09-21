ALTER TABLE blobs ADD COLUMN last_used_at_ms INTEGER NOT NULL DEFAULT 0;
-- Older versions did not track reads: grant existing content one full window.
UPDATE blobs SET last_used_at_ms = CAST(strftime('%s', 'now') AS INTEGER) * 1000;
CREATE INDEX blobs_last_used ON blobs(last_used_at_ms);
CREATE INDEX conversations_updated ON conversations(updated_at_ms);
CREATE TABLE conversation_blob_roots (
    conversation_id TEXT NOT NULL,
    blob_id BLOB NOT NULL CHECK(length(blob_id) = 32),
    PRIMARY KEY (conversation_id, blob_id)
);
