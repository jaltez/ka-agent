-- Schema v8: model-proposed skills (operator review) and the memory
-- candidate's target file (facts/preferences/environment/profile).
CREATE TABLE skill_candidates (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    description TEXT NOT NULL,
    body        TEXT NOT NULL,
    status      TEXT NOT NULL CHECK(status IN ('pending','accepted','rejected')),
    created_ms  INTEGER NOT NULL
);
CREATE INDEX idx_skill_candidates_status ON skill_candidates(status);

ALTER TABLE memory_candidates ADD COLUMN file TEXT;
