-- Schema v1: everything Ka Agent persists (SEED.md §3.5-§3.8, §8.7).
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE sessions (
    id          TEXT PRIMARY KEY,
    workspace   TEXT NOT NULL,
    strand_id   TEXT,
    strand_path TEXT,
    status      TEXT NOT NULL CHECK(status IN ('idle','running','awaiting_approval','stopping','paused','degraded')),
    created_ms  INTEGER NOT NULL,
    updated_ms  INTEGER NOT NULL
);

CREATE TABLE turns (
    id            TEXT PRIMARY KEY,
    session       TEXT NOT NULL REFERENCES sessions(id),
    inbound_text  TEXT NOT NULL,
    status        TEXT NOT NULL CHECK(status IN ('queued','submitted','running','done','error','aborted','interrupted','killed')),
    stop          TEXT,
    error_class   TEXT,
    error_message TEXT,
    cost_usd      REAL NOT NULL DEFAULT 0,
    tokens        INTEGER NOT NULL DEFAULT 0,
    created_ms    INTEGER NOT NULL,
    updated_ms    INTEGER NOT NULL
);
CREATE INDEX idx_turns_session_created ON turns(session, created_ms);

CREATE TABLE queue (
    id          TEXT PRIMARY KEY,
    session     TEXT NOT NULL REFERENCES sessions(id),
    text        TEXT NOT NULL,
    source      TEXT NOT NULL,
    enqueued_ms INTEGER NOT NULL,
    turned      TEXT
);
CREATE INDEX idx_queue_session_enqueued ON queue(session, enqueued_ms);

CREATE TABLE outbox (
    turn               TEXT NOT NULL,
    chunk_index        INTEGER NOT NULL,
    text               TEXT NOT NULL,
    status             TEXT NOT NULL DEFAULT 'queued' CHECK(status IN ('queued','sent','failed')),
    channel_message_id INTEGER,
    created_ms         INTEGER NOT NULL,
    PRIMARY KEY(turn, chunk_index)
);

CREATE TABLE approvals (
    id            TEXT PRIMARY KEY,
    session       TEXT NOT NULL,
    turn          TEXT NOT NULL,
    kind_json     TEXT NOT NULL,
    status        TEXT NOT NULL CHECK(status IN ('pending','approved','denied','expired','cancelled','superseded')),
    expires_at_ms INTEGER NOT NULL,
    decided_at_ms INTEGER,
    created_ms    INTEGER NOT NULL
);

CREATE TABLE tasks (
    id              TEXT PRIMARY KEY,
    session         TEXT,
    title           TEXT NOT NULL,
    kind            TEXT NOT NULL,
    status          TEXT NOT NULL CHECK(status IN ('queued','running','waiting_approval','succeeded','failed','cancelled','interrupted','unknown')),
    read_only       INTEGER NOT NULL DEFAULT 0,
    scheduled_at_ms INTEGER,
    created_ms      INTEGER NOT NULL,
    updated_ms      INTEGER NOT NULL
);

CREATE TABLE runs (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    ref_id        TEXT,
    status        TEXT NOT NULL,
    stop          TEXT,
    error_class   TEXT,
    error_message TEXT,
    cost_usd      REAL,
    tokens        INTEGER,
    started_ms    INTEGER NOT NULL,
    ended_ms      INTEGER
);

CREATE TABLE memory_candidates (
    id          TEXT PRIMARY KEY,
    source      TEXT NOT NULL,
    text        TEXT NOT NULL,
    provenance  TEXT,
    status      TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','accepted','rejected','discarded')),
    created_ms  INTEGER NOT NULL
);

CREATE TABLE audit (
    seq       INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms     INTEGER NOT NULL,
    actor     TEXT NOT NULL,
    action    TEXT NOT NULL,
    subject   TEXT NOT NULL,
    detail    TEXT,
    prev_hash TEXT NOT NULL,
    hash      TEXT NOT NULL
);

CREATE VIRTUAL TABLE episodes_fts USING fts5(
    session UNINDEXED,
    turn UNINDEXED,
    role,
    content
);
