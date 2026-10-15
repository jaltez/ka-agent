-- Schema v2: intents table for external-write audit (SEED.md §7.3).
CREATE TABLE intents (
    id           TEXT PRIMARY KEY,
    tool         TEXT NOT NULL,
    args_hash    TEXT NOT NULL,
    status       TEXT NOT NULL CHECK(status IN ('proposed','approved','denied','executing','done','failed','unknown')),
    external_ref TEXT,
    summary      TEXT,
    created_ms   INTEGER NOT NULL,
    updated_ms   INTEGER NOT NULL
);
CREATE INDEX idx_intents_status ON intents(status);
CREATE INDEX idx_intents_created ON intents(created_ms);
