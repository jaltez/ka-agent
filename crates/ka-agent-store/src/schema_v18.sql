-- v18: plan artifacts (v4 F12 early slice) — an editable plan per session
-- with an operator-approved verify gate and bounded repair iterations.
-- Status machine: docs/contract/plans.md.
CREATE TABLE plans (
    id             TEXT PRIMARY KEY,
    session        TEXT NOT NULL,
    username       TEXT NOT NULL,
    title          TEXT NOT NULL DEFAULT '',
    body           TEXT NOT NULL DEFAULT '',
    verify         TEXT,
    max_iterations INTEGER NOT NULL DEFAULT 3,
    iterations     INTEGER NOT NULL DEFAULT 0,
    status         TEXT NOT NULL DEFAULT 'drafting'
        CHECK (status IN ('drafting','draft','approved','executing','done','failed','abandoned')),
    draft_turn     TEXT,
    created_ms     INTEGER NOT NULL,
    updated_ms     INTEGER NOT NULL
);
CREATE INDEX idx_plans_session ON plans(session, created_ms);
