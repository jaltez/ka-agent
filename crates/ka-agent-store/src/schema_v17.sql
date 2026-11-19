-- v17: `/archive` (v4 A13) — hide a session from lists without touching
-- its transcript. `archived` rides the sessions row.

ALTER TABLE sessions ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
