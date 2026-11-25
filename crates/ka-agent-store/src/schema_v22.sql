-- v22: the source that submitted each turn. The dream extractor reads
-- session transcripts; it must exclude turns a schedule started (B15b's
-- automation boundary applies to the extraction input, not just the
-- current turn's source).
CREATE TABLE turn_origins (
    turn_id    TEXT PRIMARY KEY,
    source     TEXT NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE INDEX idx_turn_origins_source ON turn_origins(source, created_ms);
