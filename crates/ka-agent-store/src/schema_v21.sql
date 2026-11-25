-- v21: optional embedding index (B1). The Markdown memory stays the
-- source of truth; these rows are derived and can be rebuilt at any time.
CREATE TABLE memory_vectors (
    ref_id     TEXT NOT NULL,
    username   TEXT NOT NULL,
    scope      TEXT,
    model      TEXT NOT NULL,
    dim        INTEGER NOT NULL,
    vec        BLOB NOT NULL,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (ref_id, username)
);
CREATE INDEX idx_memory_vectors_user_model
    ON memory_vectors(username, model, updated_ms);

-- Model and dimension bookkeeping, plus the fingerprint of the memory
-- entries the vectors were built from. A mismatch marks the index stale.
CREATE TABLE memory_index_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
