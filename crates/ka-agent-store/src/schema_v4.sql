-- Schema v4: generalized cron (M11). A `kind = 'cron'` task carries its
-- CronSpec as JSON in the new payload column; every other kind leaves it
-- NULL. Additive only — no data changes.
ALTER TABLE tasks ADD COLUMN payload TEXT;
