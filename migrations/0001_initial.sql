PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS kb_course_nodes (
    node_id TEXT PRIMARY KEY,
    course_id TEXT NOT NULL,
    parent_node_id TEXT,
    level INTEGER NOT NULL CHECK (level IN (1, 2)),
    title TEXT NOT NULL,
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    source_updated_at TEXT,
    synced_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (parent_node_id) REFERENCES kb_course_nodes(node_id)
);

CREATE INDEX IF NOT EXISTS idx_kb_course_nodes_course_parent
    ON kb_course_nodes(course_id, parent_node_id, level, is_active);

CREATE TABLE IF NOT EXISTS kb_assets (
    asset_id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    asset_type TEXT NOT NULL,
    original_name TEXT NOT NULL,
    storage_key TEXT NOT NULL UNIQUE,
    byte_size INTEGER NOT NULL CHECK (byte_size >= 0),
    mime_type TEXT,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at TEXT,
    FOREIGN KEY (node_id) REFERENCES kb_course_nodes(node_id)
);

CREATE INDEX IF NOT EXISTS idx_kb_assets_node_active
    ON kb_assets(node_id, deleted_at);

CREATE TABLE IF NOT EXISTS kb_asset_revisions (
    revision_id TEXT PRIMARY KEY,
    asset_id TEXT NOT NULL,
    revision_number INTEGER NOT NULL CHECK (revision_number > 0),
    source_sha256 TEXT NOT NULL,
    parser_version TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('draft', 'extracting', 'review', 'indexing', 'ready', 'published', 'withdrawn', 'failed')),
    extraction_json TEXT,
    review_note TEXT,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    published_at TEXT,
    UNIQUE (asset_id, revision_number),
    FOREIGN KEY (asset_id) REFERENCES kb_assets(asset_id)
);

CREATE INDEX IF NOT EXISTS idx_kb_asset_revisions_publish
    ON kb_asset_revisions(asset_id, status, published_at);

CREATE TABLE IF NOT EXISTS kb_content_parts (
    part_id TEXT PRIMARY KEY,
    revision_id TEXT NOT NULL,
    part_index INTEGER NOT NULL CHECK (part_index >= 0),
    modality TEXT NOT NULL CHECK (modality IN ('text', 'image', 'video', 'audio', 'mixed')),
    content_text TEXT,
    media_storage_key TEXT,
    page_number INTEGER,
    start_ms INTEGER,
    end_ms INTEGER,
    metadata_json TEXT,
    content_sha256 TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (revision_id, part_index),
    FOREIGN KEY (revision_id) REFERENCES kb_asset_revisions(revision_id)
);

CREATE INDEX IF NOT EXISTS idx_kb_content_parts_revision
    ON kb_content_parts(revision_id, part_index);

CREATE VIRTUAL TABLE IF NOT EXISTS kb_content_parts_fts USING fts5(
    part_id UNINDEXED,
    content_text,
    content='kb_content_parts',
    content_rowid='rowid'
);

CREATE TABLE IF NOT EXISTS kb_embedding_configs (
    embedding_config_id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    dimensions INTEGER NOT NULL CHECK (dimensions > 0),
    instructions_fingerprint TEXT,
    config_fingerprint TEXT NOT NULL UNIQUE,
    is_active INTEGER NOT NULL DEFAULT 0 CHECK (is_active IN (0, 1)),
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS kb_vector_collections (
    collection_id TEXT PRIMARY KEY,
    course_id TEXT NOT NULL,
    embedding_config_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    qdrant_collection TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL CHECK (status IN ('building', 'active', 'retired', 'failed')),
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (embedding_config_id) REFERENCES kb_embedding_configs(embedding_config_id)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_kb_vector_collections_generation
    ON kb_vector_collections(course_id, embedding_config_id, generation);

CREATE TABLE IF NOT EXISTS kb_vector_records (
    vector_record_id TEXT PRIMARY KEY,
    collection_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    part_id TEXT NOT NULL,
    qdrant_point_id TEXT NOT NULL,
    modality TEXT NOT NULL,
    dimensions INTEGER NOT NULL CHECK (dimensions > 0),
    status TEXT NOT NULL CHECK (status IN ('pending', 'written', 'verified', 'deleted', 'failed')),
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (collection_id, qdrant_point_id),
    UNIQUE (collection_id, part_id),
    FOREIGN KEY (collection_id) REFERENCES kb_vector_collections(collection_id),
    FOREIGN KEY (revision_id) REFERENCES kb_asset_revisions(revision_id),
    FOREIGN KEY (part_id) REFERENCES kb_content_parts(part_id)
);

CREATE TABLE IF NOT EXISTS kb_jobs (
    job_id TEXT PRIMARY KEY,
    job_type TEXT NOT NULL,
    asset_id TEXT,
    revision_id TEXT,
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    error_code TEXT,
    error_message TEXT,
    available_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    started_at TEXT,
    finished_at TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (asset_id) REFERENCES kb_assets(asset_id),
    FOREIGN KEY (revision_id) REFERENCES kb_asset_revisions(revision_id)
);

CREATE INDEX IF NOT EXISTS idx_kb_jobs_queue
    ON kb_jobs(state, available_at);

CREATE TABLE IF NOT EXISTS kb_conversations (
    conversation_id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    course_id TEXT NOT NULL,
    scope_type TEXT NOT NULL CHECK (scope_type IN ('course', 'level1', 'level2')),
    scope_id TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_kb_conversations_owner
    ON kb_conversations(user_id, updated_at);

CREATE TABLE IF NOT EXISTS kb_messages (
    message_id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'system')),
    content_json TEXT NOT NULL,
    citations_json TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (conversation_id) REFERENCES kb_conversations(conversation_id)
);

CREATE INDEX IF NOT EXISTS idx_kb_messages_conversation
    ON kb_messages(conversation_id, created_at);

CREATE TABLE IF NOT EXISTS kb_model_configs (
    model_config_id TEXT PRIMARY KEY,
    purpose TEXT NOT NULL CHECK (purpose IN ('embedding', 'chat')),
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    config_json TEXT NOT NULL,
    is_active INTEGER NOT NULL DEFAULT 0 CHECK (is_active IN (0, 1)),
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS kb_model_secrets (
    model_config_id TEXT PRIMARY KEY,
    encrypted_value BLOB NOT NULL,
    key_version INTEGER NOT NULL CHECK (key_version > 0),
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (model_config_id) REFERENCES kb_model_configs(model_config_id)
);

CREATE TABLE IF NOT EXISTS kb_audit_events (
    audit_id TEXT PRIMARY KEY,
    actor_id TEXT,
    action TEXT NOT NULL,
    resource_type TEXT NOT NULL,
    resource_id TEXT,
    detail_json TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

