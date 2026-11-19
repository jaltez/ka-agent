-- v16: `/preset` slots (v4 A8) — a numbered model+mode+effort combo per user.

CREATE TABLE presets (
    username   TEXT NOT NULL,
    slot       INTEGER NOT NULL,
    model      TEXT,
    mode       TEXT,
    effort     TEXT,
    PRIMARY KEY (username, slot)
);
