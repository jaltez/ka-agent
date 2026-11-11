-- v11: in-process RBAC. Every subject row carries the role it belongs to;
-- pre-RBAC rows are admin (the implicit role of every local surface), so an
-- existing data root keeps behaving exactly as before.
ALTER TABLE identities ADD COLUMN role TEXT NOT NULL DEFAULT 'admin';
ALTER TABLE sessions ADD COLUMN role TEXT NOT NULL DEFAULT 'admin';
ALTER TABLE memory_candidates ADD COLUMN role TEXT NOT NULL DEFAULT 'admin';
ALTER TABLE audit ADD COLUMN role TEXT NOT NULL DEFAULT 'admin';
CREATE INDEX idx_sessions_role_updated ON sessions(role, updated_ms);
CREATE INDEX idx_candidates_role_status ON memory_candidates(role, status);
