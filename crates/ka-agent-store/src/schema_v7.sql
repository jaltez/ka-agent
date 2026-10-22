-- Schema v7: CronSpec's single `weekday` became `weekdays` (a list, empty
-- = every day; plus `month_day`). Rewrite stored payloads in place so
-- existing weekly crons keep firing on their day.
UPDATE tasks
SET payload = json_remove(
        json_set(payload, '$.weekdays', json_array(json_extract(payload, '$.weekday'))),
        '$.weekday')
WHERE kind = 'cron'
  AND json_valid(payload)
  AND json_type(payload, '$.weekday') = 'integer';
