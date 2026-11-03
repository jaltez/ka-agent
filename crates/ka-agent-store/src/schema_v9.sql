-- Schema v9: the model a turn ran on, recorded at intake, so `/usage` and
-- `ka-agent usage` can split spend per model (v2 audit E5). NULL for rows
-- written before this migration.
ALTER TABLE turns ADD COLUMN model TEXT;
