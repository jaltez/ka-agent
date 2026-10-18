-- Schema v5: the task status set gains 'missed' (SEED.md §7.2: a
-- scheduled fire overdue beyond the grace hour is recorded, reported and
-- never fired). SQLite cannot ALTER a CHECK constraint, so the table is
-- rebuilt with the same shape plus the new value, data copied in place.
CREATE TABLE tasks_new (
    id              TEXT PRIMARY KEY,
    session         TEXT,
    title           TEXT NOT NULL,
    kind            TEXT NOT NULL,
    status          TEXT NOT NULL CHECK(status IN ('queued','running','waiting_approval','succeeded','failed','cancelled','interrupted','unknown','missed')),
    read_only       INTEGER NOT NULL DEFAULT 0,
    scheduled_at_ms INTEGER,
    payload         TEXT,
    created_ms      INTEGER NOT NULL,
    updated_ms      INTEGER NOT NULL
);
INSERT INTO tasks_new (id, session, title, kind, status, read_only, scheduled_at_ms, payload, created_ms, updated_ms)
    SELECT id, session, title, kind, status, read_only, scheduled_at_ms, payload, created_ms, updated_ms FROM tasks;
DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;
