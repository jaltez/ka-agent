//! Cron job operations straight on the store — shared by the MCP bridge
//! (`cron_manage`) and `ka-agent cron`, which both run outside the daemon.
//! The daemon's scheduler picks every change up on its next tick.

use ka_agent_core::{CronSpec, Task};
use ka_agent_store::Store;

/// The one live cron whose id matches `query` (full id, prefix, or the
/// short tail surfaces show). Ambiguity is refused, never guessed.
pub fn resolve(store: &Store, query: &str) -> Result<Task, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("missing id".into());
    }
    let crons = store.cron_tasks().map_err(|e| format!("store: {e}"))?;
    let mut hits = crons
        .into_iter()
        .filter(|t| ka_agent_core::ids::id_matches(t.id.as_str(), query));
    let task = hits
        .next()
        .ok_or_else(|| format!("no live cron matches {query:?}"))?;
    if hits.next().is_some() {
        return Err(format!(
            "{query:?} matches several crons — use more of the id (see the list)"
        ));
    }
    Ok(task)
}

/// Pause (kept, not fired until resumed).
pub fn pause(store: &Store, task: &Task) -> Result<(), String> {
    store
        .set_task_status(&task.id, "interrupted")
        .map_err(|e| format!("pause: {e}"))
}

/// Remove for good.
pub fn remove(store: &Store, task: &Task) -> Result<(), String> {
    store
        .set_task_status(&task.id, "cancelled")
        .map_err(|e| format!("remove: {e}"))
}

/// Fire on the next tick (then the job re-arms on its usual schedule).
pub fn trigger(store: &Store, task: &Task) -> Result<(), String> {
    store
        .update_task_schedule(&task.id, Some(ka_agent_core::now_ms()), "queued")
        .map_err(|e| format!("trigger: {e}"))
}

/// Resume a paused job at its next occurrence strictly after `now`
/// (missed ones are skipped); a one-shot fires on the next tick. Returns
/// the new fire time.
pub fn resume(store: &Store, task: &Task, now: &jiff::Zoned) -> Result<i64, String> {
    let now_ms = now.timestamp().as_millisecond();
    let spec = task.payload.as_deref().and_then(CronSpec::from_payload);
    let next = match &spec {
        Some(s) if s.at_local.is_some() => {
            let (hh, mm) = s
                .at_local
                .as_deref()
                .and_then(ka_agent_core::when::parse_hhmm)
                .ok_or("resume: bad stored time")?;
            ka_agent_core::when::next_local(hh, mm, &s.day_rule(), now)
        }
        Some(s) if s.recurrence_ms.is_some_and(|r| r > 0) => {
            let rec = s.recurrence_ms.unwrap_or(1);
            let mut n = task.scheduled_at_ms.unwrap_or(now_ms);
            if n <= now_ms {
                // Jump past every missed occurrence at once.
                n += ((now_ms - n) / rec + 1) * rec;
            }
            n
        }
        _ => now_ms,
    };
    store
        .update_task_schedule(&task.id, Some(next), "queued")
        .map_err(|e| format!("resume: {e}"))?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn resume_finds_the_next_valid_day_for_anchored_jobs() {
        let dir = std::env::temp_dir().join(format!(
            "ka-cron-ops-{}-{}",
            std::process::id(),
            ka_agent_core::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("state.db")).unwrap();
        // Thu 29 Oct, 10:00 Madrid; a weekday-only 09:00 job paused long ago.
        let now: jiff::Zoned = "2026-10-29T10:00:00+01:00[Europe/Madrid]".parse().unwrap();
        let (spec, _) =
            CronSpec::from_words(&["every", "mon,wed", "at", "9", "plan"], "operator", &now)
                .unwrap();
        let mut task = Task::new(spec.text.clone(), "cron", false);
        task.scheduled_at_ms = Some(1);
        task.payload = Some(spec.to_payload());
        store.insert_task(&task).unwrap();
        let t = resolve(&store, ka_agent_core::ids::short_id(task.id.as_str())).unwrap();
        pause(&store, &t).unwrap();
        let next = resume(&store, &t, &now).unwrap();
        let z = jiff::Timestamp::from_millisecond(next)
            .unwrap()
            .to_zoned(now.time_zone().clone());
        // Next Monday (2 Nov) 09:00 — not "a day later than the old slot".
        assert_eq!((z.month(), z.day(), z.hour()), (11, 2, 9));
        assert!(resolve(&store, "nope").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
