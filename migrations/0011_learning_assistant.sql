ALTER TABLE kb_assets ADD COLUMN evidence_mode TEXT NOT NULL DEFAULT 'content';
ALTER TABLE kb_conversations ADD COLUMN creator_role TEXT NOT NULL DEFAULT 'student';

-- Student learning records are an administrative corpus, never course evidence.
CREATE TABLE kb_learning_state (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    synced_at TEXT,
    last_error TEXT,
    generation INTEGER NOT NULL DEFAULT 0,
    data_cutoff_rowid INTEGER NOT NULL DEFAULT 0,
    vector_cutoff_rowid INTEGER NOT NULL DEFAULT 0
);
INSERT INTO kb_learning_state(singleton) VALUES(1);
CREATE TABLE kb_learning_turns (
    turn_id TEXT PRIMARY KEY,
    source_rowid INTEGER NOT NULL,
    conversation_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    course_id TEXT NOT NULL,
    scope_type TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    question TEXT NOT NULL,
    answer TEXT NOT NULL,
    citations_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    byte_size INTEGER NOT NULL,
    indexed_fingerprint TEXT,
    vector_bytes INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_kb_learning_scope ON kb_learning_turns(course_id,scope_type,scope_id,created_at);
CREATE TABLE kb_learning_collections (
    fingerprint TEXT PRIMARY KEY,
    collection_name TEXT NOT NULL UNIQUE,
    dimensions INTEGER NOT NULL
);
CREATE TABLE kb_teacher_conversations (
    conversation_id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    scope_type TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    title TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE kb_teacher_messages (
    message_id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES kb_teacher_conversations(conversation_id) ON DELETE CASCADE,
    client_message_id TEXT NOT NULL,
    question TEXT NOT NULL,
    answer TEXT NOT NULL,
    sources_json TEXT NOT NULL,
    elapsed_ms INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(conversation_id,client_message_id)
);
