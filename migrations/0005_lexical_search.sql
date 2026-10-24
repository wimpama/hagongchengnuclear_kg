CREATE VIRTUAL TABLE kb_lexical_fts USING fts5(part_id UNINDEXED, tokens);
CREATE TABLE kb_lexical_versions(part_id TEXT PRIMARY KEY REFERENCES kb_content_parts(part_id), content_sha256 TEXT NOT NULL);
