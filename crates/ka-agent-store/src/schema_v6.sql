-- Schema v6: indexes for the 15 s maintenance-tick queries. `tasks` and
-- `approvals` had none, so due_tasks/next_due_ms/expire_pending/cron_tasks
-- full-scanned on every tick and only got slower as rows accumulated.
CREATE INDEX idx_tasks_status_scheduled ON tasks(status, scheduled_at_ms);
CREATE INDEX idx_tasks_kind_status ON tasks(kind, status);
CREATE INDEX idx_approvals_status_expires ON approvals(status, expires_at_ms);
CREATE INDEX idx_approvals_session_status ON approvals(session, status);
