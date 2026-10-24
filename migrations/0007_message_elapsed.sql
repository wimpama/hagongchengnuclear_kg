ALTER TABLE kb_messages ADD COLUMN elapsed_ms INTEGER CHECK (elapsed_ms IS NULL OR elapsed_ms >= 0);
