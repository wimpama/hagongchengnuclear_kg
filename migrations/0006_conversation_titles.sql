ALTER TABLE kb_conversations ADD COLUMN title TEXT NOT NULL DEFAULT '';

CREATE INDEX IF NOT EXISTS idx_kb_conversations_title
    ON kb_conversations(conversation_id, title);
