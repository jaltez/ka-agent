-- v12: the team edition's user model (ADR-0004). A channel identity now
-- resolves to a *user* (a person); the user's role stays the permission
-- set. Every partition key gains `username`; the backfill maps the existing
-- role-keyed rows through the same rule the bootstrap uses:
--   role 'admin' -> user 'operator'; any other role -> a user named after it.
-- An existing single-user install therefore ends with exactly one user
-- 'operator' and identical behavior.

CREATE TABLE users (
    username        TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    pw_hash         TEXT,                          -- argon2id PHC; NULL until an invite is redeemed
    role            TEXT NOT NULL DEFAULT 'admin', -- names a [[role]]; 'admin' allowed
    quota_daily_usd REAL,                          -- NULL = the role/profile budget applies
    disabled        INTEGER NOT NULL DEFAULT 0,
    created_ms      INTEGER NOT NULL,
    created_by      TEXT
);

CREATE TABLE groups (
    name           TEXT PRIMARY KEY,
    models_allow   TEXT,   -- JSON array of globs ('anthropic/*'); NULL/[] = every catalog model
    models_default TEXT    -- 'openai/gpt-5.2'; NULL = the profile default
);

CREATE TABLE users_groups (
    username   TEXT NOT NULL REFERENCES users(username),
    group_name TEXT NOT NULL REFERENCES groups(name),
    PRIMARY KEY(username, group_name)
);

CREATE TABLE web_sessions (
    token_hash TEXT PRIMARY KEY,
    username   TEXT NOT NULL REFERENCES users(username),
    created_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL
);
CREATE INDEX idx_web_sessions_user ON web_sessions(username);

CREATE TABLE invites (
    code_hash  TEXT PRIMARY KEY,
    role       TEXT NOT NULL,
    -- NULL = creates a new account; a username = a password reset for that
    -- existing account (the admin's reset flow).
    username   TEXT,
    groups     TEXT,   -- JSON array of group names
    created_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL,
    used_ms    INTEGER,
    created_by TEXT NOT NULL
);

CREATE TABLE kbs (
    name           TEXT PRIMARY KEY,
    display_name   TEXT NOT NULL,
    owner_username TEXT NOT NULL REFERENCES users(username),
    created_ms     INTEGER NOT NULL
);

CREATE TABLE kb_docs (
    kb_name TEXT NOT NULL REFERENCES kbs(name),
    doc_id  INTEGER PRIMARY KEY AUTOINCREMENT,
    path    TEXT NOT NULL,
    title   TEXT NOT NULL,
    content TEXT NOT NULL,
    mtime_ms INTEGER NOT NULL,
    sha256  TEXT NOT NULL,
    source  TEXT NOT NULL,   -- 'upload' | 'notion' | 'dropbox' | 'git'
    UNIQUE(kb_name, path)
);

CREATE TABLE kb_access (
    kb_name    TEXT NOT NULL REFERENCES kbs(name),
    group_name TEXT NOT NULL,
    PRIMARY KEY(kb_name, group_name)
);

-- Standalone FTS5 mirror (the episodes_fts pattern: explicit deletes, no
-- triggers). `kb_name`/`path` are UNINDEXED but carried so a hit can be
-- cited and a replaced/deleted document can be re-indexed exactly.
CREATE VIRTUAL TABLE kb_docs_fts USING fts5(
    kb_name UNINDEXED,
    path UNINDEXED,
    title,
    content
);

-- Knowledge sources (M3): a KB fed by an external system on a schedule.
-- `cursor` is the connector's resume token (Dropbox's cursor; Notion has
-- none; git ignores it).
CREATE TABLE kb_sources (
    kb_name       TEXT NOT NULL REFERENCES kbs(name),
    kind          TEXT NOT NULL,   -- 'notion' | 'dropbox' | 'git'
    target        TEXT NOT NULL,   -- root page/database id, folder path, git URL
    token_secret  TEXT,            -- secret-store name for the source token
    include_globs TEXT,            -- JSON array; NULL = every supported file
    cursor        TEXT,
    schedule_ms   INTEGER NOT NULL,
    created_ms    INTEGER NOT NULL,
    PRIMARY KEY(kb_name, kind, target)
);

-- Provenance (M3 share-time check): which KBs/files/urls fed a turn, and
-- which groups may see each source. `turn_id` is the turns.id string (the
-- plan's INTEGER referred to a numeric row id; turns are keyed by a TEXT id
-- in this schema, so the column is TEXT to stay a real reference).
CREATE TABLE turn_sources (
    turn_id     TEXT NOT NULL,
    kind        TEXT NOT NULL,   -- 'kb' | 'file' | 'url'
    ref         TEXT NOT NULL,   -- 'kb:docs/path.md', a workspace path, a URL
    group_names TEXT,            -- JSON array of the groups that may see it
    PRIMARY KEY(turn_id, kind, ref)
);

-- Shared agents (M3): a definition (persona + skills + KBs + model) shared
-- with groups; run state stays per user.
CREATE TABLE agents (
    name           TEXT PRIMARY KEY,
    persona        TEXT NOT NULL,
    model          TEXT,
    skills         TEXT NOT NULL,   -- JSON array
    kbs            TEXT NOT NULL,   -- JSON array
    owner_username TEXT NOT NULL REFERENCES users(username),
    created_ms     INTEGER NOT NULL
);
CREATE TABLE agent_access (
    agent_name TEXT NOT NULL REFERENCES agents(name),
    group_name TEXT NOT NULL,
    level      TEXT NOT NULL CHECK(level IN ('viewer','editor','owner')),
    PRIMARY KEY(agent_name, group_name)
);

-- ---- alterations -------------------------------------------------------

ALTER TABLE identities ADD COLUMN username TEXT REFERENCES users(username);
ALTER TABLE sessions ADD COLUMN username TEXT REFERENCES users(username);
ALTER TABLE memory_candidates ADD COLUMN username TEXT NOT NULL DEFAULT 'operator';
ALTER TABLE audit ADD COLUMN username TEXT NOT NULL DEFAULT 'operator';
ALTER TABLE tasks ADD COLUMN username TEXT;
ALTER TABLE approvals ADD COLUMN username TEXT;
ALTER TABLE approvals ADD COLUMN escalated INTEGER NOT NULL DEFAULT 0;

CREATE INDEX idx_sessions_user_updated ON sessions(username, updated_ms);
CREATE INDEX idx_candidates_user_status ON memory_candidates(username, status);
CREATE INDEX idx_tasks_username ON tasks(username);

-- ---- backfill ----------------------------------------------------------

INSERT INTO users(username, display_name, role, created_ms, created_by)
VALUES ('operator', 'operator', 'admin', 0, 'schema_v12');

-- One user per distinct non-admin role that has at least one identity OR
-- one session (pre-users role switching created sessions without any
-- identity; the FK on sessions.username needs the row to exist).
INSERT OR IGNORE INTO users(username, display_name, role, created_ms, created_by)
SELECT DISTINCT role, role, role, 0, 'schema_v12'
FROM identities
WHERE role <> 'admin' AND role <> ''
UNION
SELECT DISTINCT role, role, role, 0, 'schema_v12'
FROM sessions
WHERE role <> 'admin' AND role <> '';

UPDATE identities
SET username = CASE WHEN role = 'admin' THEN 'operator' ELSE role END;

UPDATE sessions
SET username = CASE WHEN role = 'admin' THEN 'operator' ELSE role END;

UPDATE memory_candidates
SET username = CASE WHEN role = 'admin' THEN 'operator' ELSE role END;

UPDATE audit
SET username = CASE WHEN role = 'admin' THEN 'operator' ELSE role END;

-- Cron rows carry their creator's role in the payload (v4+); map it through
-- the same rule. Pre-v4 reminder rows (no role) stay NULL = operator.
UPDATE tasks
SET username = CASE
        WHEN json_valid(payload) AND json_type(payload, '$.role') = 'text'
             AND json_extract(payload, '$.role') <> 'admin'
            THEN json_extract(payload, '$.role')
        ELSE 'operator'
    END
WHERE kind = 'cron';

-- And stamp the payload itself, so a fired cron resolves its creator the
-- same way the column does (pre-users payloads carry no username).
UPDATE tasks
SET payload = json_set(payload, '$.username', json_extract(payload, '$.username'))
WHERE kind = 'cron'
  AND json_valid(payload)
  AND json_type(payload, '$.username') = 'text';

UPDATE tasks
SET payload = json_set(
        payload,
        '$.username',
        CASE
            WHEN json_extract(payload, '$.role') = 'admin'
                OR json_extract(payload, '$.role') IS NULL
            THEN 'operator'
            ELSE json_extract(payload, '$.role')
        END
     )
WHERE kind = 'cron'
  AND json_valid(payload)
  AND json_type(payload, '$.username') IS NULL;
