-- v20: searchable accepted memory (A). One row per accepted bullet; the
-- runtime rebuilds a user's rows from the Markdown source of truth. The
-- FTS table is derived state, never the source of truth.
CREATE VIRTUAL TABLE memory_fts USING fts5(
    id UNINDEXED,
    username UNINDEXED,
    file UNINDEXED,
    text,
    keywords,
    source UNINDEXED,
    accepted_ms UNINDEXED,
    valid_until UNINDEXED
);
