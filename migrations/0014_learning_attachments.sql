-- Keep references to student uploads, not a second copy of their private content.
-- Existing text-only vectors remain valid; live attachments trigger a replacement.
ALTER TABLE kb_learning_turns ADD COLUMN attachments_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE kb_learning_vectors ADD COLUMN media_key TEXT NOT NULL DEFAULT '';
