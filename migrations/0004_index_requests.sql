CREATE TABLE kb_index_requests (
    job_id TEXT PRIMARY KEY REFERENCES kb_jobs(job_id),
    revision_id TEXT NOT NULL REFERENCES kb_asset_revisions(revision_id),
    config_fingerprint TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(revision_id, config_fingerprint)
);
CREATE INDEX idx_kb_index_request_revision ON kb_index_requests(revision_id);

-- FTS is a derived index. Every search must still JOIN the current publication
-- pointer and authorize the node; indexing text does not make it visible.
CREATE TRIGGER kb_parts_fts_insert AFTER INSERT ON kb_content_parts BEGIN
    INSERT INTO kb_content_parts_fts(rowid,part_id,content_text)
    VALUES(new.rowid,new.part_id,new.content_text);
END;
CREATE TRIGGER kb_parts_fts_delete AFTER DELETE ON kb_content_parts BEGIN
    INSERT INTO kb_content_parts_fts(kb_content_parts_fts,rowid,part_id,content_text)
    VALUES('delete',old.rowid,old.part_id,old.content_text);
END;
CREATE TRIGGER kb_parts_fts_update AFTER UPDATE ON kb_content_parts BEGIN
    INSERT INTO kb_content_parts_fts(kb_content_parts_fts,rowid,part_id,content_text)
    VALUES('delete',old.rowid,old.part_id,old.content_text);
    INSERT INTO kb_content_parts_fts(rowid,part_id,content_text)
    VALUES(new.rowid,new.part_id,new.content_text);
END;
INSERT INTO kb_content_parts_fts(kb_content_parts_fts) VALUES('rebuild');
