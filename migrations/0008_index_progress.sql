ALTER TABLE kb_jobs ADD COLUMN progress_completed INTEGER NOT NULL DEFAULT 0 CHECK (progress_completed >= 0);
ALTER TABLE kb_jobs ADD COLUMN progress_total INTEGER NOT NULL DEFAULT 0 CHECK (progress_total >= 0);
ALTER TABLE kb_jobs ADD COLUMN progress_stage TEXT NOT NULL DEFAULT 'queued';
