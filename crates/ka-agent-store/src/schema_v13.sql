-- v13: durable allow-rules (v4 B7). `/y <id> always` persists the shape
-- here; the runtime consults the subject's rules at ask time, loaded
-- fresh every time (never cached — the MCP-bridge rule).

CREATE TABLE grant_rules (
    id             TEXT PRIMARY KEY,
    subject        TEXT NOT NULL,            -- acting username (partition key)
    kind           TEXT NOT NULL,            -- ask_key | tool | command
    pattern        TEXT NOT NULL,            -- exact question (ask_key), tool name, or command prefix
    detail         TEXT,                     -- ask_key's detail half
    mode           TEXT NOT NULL,            -- canonical mode at creation
    from_approval  TEXT,                     -- approval that created it
    created_ms     INTEGER NOT NULL
);

CREATE INDEX grant_rules_subject ON grant_rules(subject);
