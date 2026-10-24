ALTER TABLE kb_conversations ADD COLUMN deleted_at TEXT;
CREATE TABLE kb_blob_cleanup (
    storage_key TEXT PRIMARY KEY,
    attempts INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE kb_instance_environment (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    environment TEXT NOT NULL
);
-- Preserve attachment creation times; extend the existing policy to 30 days.
UPDATE kb_question_attachments SET expires_at=datetime(created_at,'+30 days');
