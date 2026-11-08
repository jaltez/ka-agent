-- v10: private (incognito) sessions. The flag is set at insert and never
-- updated; turn rows for private sessions carry no text, so nothing here
-- backfills.
ALTER TABLE sessions ADD COLUMN private INTEGER NOT NULL DEFAULT 0;
