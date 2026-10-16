-- Schema v3: linked channel identities (SEED.md §8.1, Phase 6). Exactly one
-- operator per instance; each row links one channel identity to that
-- operator. Pairing is CLI-only and audited.
CREATE TABLE identities (
    channel    TEXT NOT NULL,          -- telegram | matrix | …
    address    TEXT NOT NULL,          -- numeric id, @user:domain, …
    linked_ms  INTEGER NOT NULL,
    via        TEXT NOT NULL,          -- cli | config-migration | …
    PRIMARY KEY (channel, address)
);
