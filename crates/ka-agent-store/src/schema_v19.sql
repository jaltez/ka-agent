-- v19: webhook-trigger idempotency (ADR-0005 stage 2) — at-least-once
-- delivery deduplicated by the sender's event id (header or body):
-- insert-or-ignore decides; a replay answers {"ok":true,"dup":true} and
-- arms nothing.
CREATE TABLE trigger_events (
    event_id   TEXT PRIMARY KEY,
    trigger_id TEXT NOT NULL,
    seen_ms    INTEGER NOT NULL
);
CREATE INDEX idx_trigger_events_trigger ON trigger_events(trigger_id, seen_ms);
