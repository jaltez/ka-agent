-- v15: skill attach bookkeeping (B12). One row per (user, skill): how many
-- turns attached it and by which route (explicit `/skill`, a keyword
-- trigger, or a parked `paths` touch). `/skill-doctor` reads these; they
-- are counts, never content — a skill stays operator-owned text.

CREATE TABLE skill_usage (
    username     TEXT NOT NULL,
    skill        TEXT NOT NULL,
    explicit     INTEGER NOT NULL DEFAULT 0,
    trigger      INTEGER NOT NULL DEFAULT 0,
    paths        INTEGER NOT NULL DEFAULT 0,
    last_used_ms INTEGER NOT NULL,
    PRIMARY KEY(username, skill)
);
