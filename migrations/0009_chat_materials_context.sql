ALTER TABLE kb_messages ADD COLUMN presentation_json TEXT;
ALTER TABLE kb_messages ADD COLUMN attachments_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE kb_conversations ADD COLUMN context_summary TEXT NOT NULL DEFAULT '';
ALTER TABLE kb_conversations ADD COLUMN summary_through_rowid INTEGER NOT NULL DEFAULT 0;

CREATE TABLE kb_question_attachments (
    attachment_id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES kb_conversations(conversation_id),
    user_id TEXT NOT NULL,
    original_name TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    storage_key TEXT NOT NULL UNIQUE,
    byte_size INTEGER NOT NULL,
    extracted_text TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TEXT NOT NULL DEFAULT (datetime('now', '+7 days'))
);
CREATE INDEX idx_kb_question_attachments_owner ON kb_question_attachments(conversation_id, user_id, expires_at);
