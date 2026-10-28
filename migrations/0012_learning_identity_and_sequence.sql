-- Keep 0011 immutable: installations already applied its original checksum.
-- Message rowids may be reused after deletion. A durable sequence keeps cleanup
-- cutoffs stable, including when upgrading existing dialogue snapshots.
CREATE TABLE kb_learning_source_seq (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    turn_id TEXT NOT NULL UNIQUE
);
INSERT INTO kb_learning_source_seq(turn_id)
    SELECT message_id FROM kb_messages WHERE role='user' ORDER BY rowid;
CREATE TRIGGER kb_learning_capture_seq AFTER INSERT ON kb_messages WHEN new.role='user' BEGIN
    INSERT INTO kb_learning_source_seq(turn_id) VALUES(new.message_id);
END;

-- 0011 assigned a student default to conversations that predated identity
-- tracking. Preserve explicit administrator roles and student identities
-- recorded after 0011; older unverified histories wait for authenticated access.
-- Adding a replacement column lets SQLite change its default without rebuilding
-- the conversation table or disturbing message/attachment foreign keys.
ALTER TABLE kb_conversations RENAME COLUMN creator_role TO creator_role_legacy;
ALTER TABLE kb_conversations ADD COLUMN creator_role TEXT NOT NULL DEFAULT 'unknown';
UPDATE kb_conversations SET creator_role = CASE
    WHEN creator_role_legacy='admin' THEN 'admin'
    WHEN creator_role_legacy='student'
        AND julianday(created_at) > (
            SELECT julianday(installed_on) FROM _sqlx_migrations WHERE version=11
        ) THEN 'student'
    ELSE 'unknown'
END;
ALTER TABLE kb_conversations DROP COLUMN creator_role_legacy;

CREATE TABLE kb_learning_vectors (
    turn_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    source_rowid INTEGER NOT NULL,
    conversation_id TEXT NOT NULL,
    course_id TEXT NOT NULL,
    scope_type TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    vector_bytes INTEGER NOT NULL,
    PRIMARY KEY(turn_id,fingerprint)
);
CREATE TABLE kb_learning_resets (
    scope_type TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    data_cutoff INTEGER NOT NULL DEFAULT 0,
    vector_cutoff INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(scope_type,scope_id)
);

-- Translate the old global rowid cutoffs before normalizing saved snapshots.
INSERT INTO kb_learning_resets(scope_type,scope_id,data_cutoff,vector_cutoff)
SELECT 'all','',
    COALESCE((SELECT MAX(s.seq) FROM kb_learning_source_seq s
        JOIN kb_messages m ON m.message_id=s.turn_id WHERE m.rowid<=state.data_cutoff_rowid),0),
    COALESCE((SELECT MAX(s.seq) FROM kb_learning_source_seq s
        JOIN kb_messages m ON m.message_id=s.turn_id WHERE m.rowid<=state.vector_cutoff_rowid),0)
FROM kb_learning_state state
WHERE singleton=1 AND (data_cutoff_rowid>0 OR vector_cutoff_rowid>0);

UPDATE kb_learning_turns SET source_rowid = COALESCE(
    (SELECT seq FROM kb_learning_source_seq WHERE turn_id=kb_learning_turns.turn_id),0
);
INSERT INTO kb_learning_vectors(
    turn_id,fingerprint,source_rowid,conversation_id,course_id,scope_type,scope_id,vector_bytes
)
SELECT turn_id,indexed_fingerprint,source_rowid,conversation_id,course_id,scope_type,scope_id,vector_bytes
FROM kb_learning_turns WHERE indexed_fingerprint IS NOT NULL AND vector_bytes>0;
