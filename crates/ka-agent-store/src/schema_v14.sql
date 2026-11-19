-- v14: shadow-git checkpoints (B1). One row per Allow-settled ask: the
-- snapshot `/rewind` restores. `sha` NULL = the snapshot failed (the
-- write still ran — the checkpoint is the net, not a gate); such rows
-- show as holes in the list and cannot be restored.

CREATE TABLE checkpoints (
    id         TEXT PRIMARY KEY,
    session    TEXT NOT NULL,
    turn       TEXT NOT NULL,
    username   TEXT NOT NULL,            -- partition key (repo dir owner)
    approval   TEXT,                     -- NULL for session/rule settles (no card existed)
    question   TEXT,                     -- the KaAsk question, for the re-propose reply
    sha        TEXT,                     -- NULL = snapshot failed
    created_ms INTEGER NOT NULL
);

CREATE INDEX checkpoints_session ON checkpoints(session, created_ms);
