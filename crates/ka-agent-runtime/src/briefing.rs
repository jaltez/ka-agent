//! The daily briefing: one morning message instead of a heartbeat. An
//! agent-mode cron (`source = "briefing"`) whose prompt is completed at
//! fire time with what the model cannot see on its own — today's
//! reminders, what waits for the operator, calendar digests — and that must
//! say "Nothing notable today." rather than invent something. Research on
//! proactive assistants: a few well-timed messages a day help, untuned
//! nudges read as spam.

use ka_agent_core::{CronMode, CronSpec, Task, TaskId};
use ka_agent_store::Store;

/// `CronSpec::source` of the briefing job (one per instance).
pub const SOURCE: &str = "briefing";

/// The fixed part of the prompt; [`crate::Runtime`] appends today's context.
pub const PROMPT: &str = "Daily briefing for the operator. In at most 12 short lines: \
the day's calendar, reminders due today, anything waiting for them (approvals, memory \
to review), and — only if you can search the web and know their interests — up to 3 \
relevant headlines with links. Lead with what needs action. If nothing is notable, \
reply exactly: Nothing notable today. Do not ask questions.";

fn is_briefing(t: &Task) -> bool {
    t.payload
        .as_deref()
        .and_then(CronSpec::from_payload)
        .is_some_and(|s| s.source == SOURCE)
}

/// The live briefing job, if any.
pub fn find(store: &Store) -> Result<Option<Task>, ka_agent_store::StoreError> {
    Ok(store.cron_tasks()?.into_iter().find(is_briefing))
}

/// Schedule the briefing daily at `hh:mm` local time, replacing any
/// previous one. Returns the job id and its first fire time.
pub fn enable(store: &Store, at_local: &str, now: &jiff::Zoned) -> Result<(TaskId, i64), String> {
    let (hh, mm) = ka_agent_core::when::parse_hhmm(at_local)
        .ok_or_else(|| format!("bad time {at_local:?} — use HH:MM (24h), e.g. 07:30"))?;
    disable(store).map_err(|e| e.to_string())?;
    let spec = CronSpec {
        text: "daily briefing".into(),
        source: SOURCE.into(),
        recurrence_ms: Some(86_400_000),
        mode: CronMode::Agent,
        at_local: Some(format!("{hh:02}:{mm:02}")),
        weekdays: Vec::new(),
        month_day: None,
    };
    let first_at =
        ka_agent_core::when::next_local(hh, mm, &ka_agent_core::when::DayRule::Daily, now);
    let mut task = Task::new(spec.text.clone(), "cron", false);
    task.scheduled_at_ms = Some(first_at);
    task.payload = Some(spec.to_payload());
    store.insert_task(&task).map_err(|e| e.to_string())?;
    Ok((task.id, first_at))
}

/// Remove the briefing; `true` when there was one.
pub fn disable(store: &Store) -> Result<bool, ka_agent_store::StoreError> {
    let mut removed = false;
    for t in store.cron_tasks()?.into_iter().filter(is_briefing) {
        store.set_task_status(&t.id, "cancelled")?;
        removed = true;
    }
    Ok(removed)
}

/// Arm the briefing for the next scheduler tick (it then re-arms for its
/// usual time); `false` when no briefing is set up.
pub fn trigger_now(store: &Store) -> Result<bool, ka_agent_store::StoreError> {
    match find(store)? {
        Some(t) => {
            store.update_task_schedule(&t.id, Some(ka_agent_core::now_ms()), "queued")?;
            Ok(true)
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn store() -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ka-briefing-{}-{}",
            std::process::id(),
            ka_agent_core::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (Store::open(&dir.join("state.db")).unwrap(), dir)
    }

    #[test]
    fn one_briefing_at_a_time_and_off_removes_it() {
        let (s, dir) = store();
        let now: jiff::Zoned = "2026-10-30T10:00:00+01:00[Europe/Madrid]".parse().unwrap();
        let (_, first) = enable(&s, "08:00", &now).unwrap();
        // 08:00 already passed today → tomorrow 08:00 local.
        let at = jiff::Timestamp::from_millisecond(first)
            .unwrap()
            .to_zoned(now.time_zone().clone());
        assert_eq!((at.day(), at.hour(), at.minute()), (31, 8, 0));
        enable(&s, "7:30", &now).unwrap();
        let live: Vec<_> = s
            .cron_tasks()
            .unwrap()
            .into_iter()
            .filter(is_briefing)
            .collect();
        assert_eq!(live.len(), 1, "re-enabling replaces");
        let spec = CronSpec::from_payload(live[0].payload.as_deref().unwrap()).unwrap();
        assert_eq!(spec.at_local.as_deref(), Some("07:30"));
        assert!(trigger_now(&s).unwrap());
        assert!(disable(&s).unwrap());
        assert!(find(&s).unwrap().is_none());
        assert!(!trigger_now(&s).unwrap());
        assert!(enable(&s, "25:00", &now).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
