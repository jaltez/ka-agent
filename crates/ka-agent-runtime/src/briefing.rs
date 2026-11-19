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
///
/// Baked verbatim into the job's `spec.text` at creation (B13): a cron job
/// runs the text snapshotted then, so a daemon upgrade that edits this
/// constant never silently rewrites what existing jobs ask the model.
pub const PROMPT: &str = "Daily briefing for the operator. In at most 12 short lines: \
the day's calendar, reminders due today, anything waiting for them (approvals, memory \
to review), and — only if you can search the web and know their interests — up to 3 \
relevant headlines with links. Lead with what needs action. If nothing needs the \
operator today, reply exactly [SILENT]. Do not ask questions.";

/// The task title of the briefing job (short for lists; the full prompt
/// lives in the `CronSpec` payload).
const TITLE: &str = "daily briefing";

/// The placeholder `spec.text` of pre-B13 briefing jobs (their fire path
/// re-read the live [`PROMPT`] constant). The only payload shape the boot
/// migration rewrites.
const LEGACY_PLACEHOLDER: &str = "daily briefing";

/// The prompt a briefing fire submits: the creation-time snapshot plus
/// what the model cannot see on its own (time data, not instructions).
pub(crate) fn fire_prompt(spec: &CronSpec, context: &str) -> String {
    format!("{}\n\n{}", spec.text, context)
}

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
        text: PROMPT.into(),
        source: SOURCE.into(),
        recurrence_ms: Some(86_400_000),
        mode: CronMode::Agent,
        at_local: Some(format!("{hh:02}:{mm:02}")),
        weekdays: Vec::new(),
        month_day: None,
        gate: None,
        role: None,
        username: None,
        tools: Vec::new(),
    };
    let first_at =
        ka_agent_core::when::next_local(hh, mm, &ka_agent_core::when::DayRule::Daily, now);
    let mut task = Task::new(TITLE.into(), "cron", false);
    task.scheduled_at_ms = Some(first_at);
    task.payload = Some(spec.to_payload());
    store.insert_task(&task).map_err(|e| e.to_string())?;
    Ok((task.id, first_at))
}

/// One-time boot migration (B13): pre-B13 briefing jobs stored only the
/// [`LEGACY_PLACEHOLDER`] and re-read the live [`PROMPT`] at fire time.
/// Snapshot the current constant into their payloads so they too become
/// immutable. Returns how many payloads were rewritten; rows already
/// holding a snapshot are left alone, which makes this self-terminating
/// (and never rewrites a snapshot taken under an older constant).
pub fn snapshot_legacy_prompts(store: &Store) -> Result<usize, ka_agent_store::StoreError> {
    let mut migrated = 0;
    for t in store.cron_tasks()?.into_iter().filter(is_briefing) {
        let Some(mut spec) = t.payload.as_deref().and_then(CronSpec::from_payload) else {
            continue;
        };
        if spec.text != LEGACY_PLACEHOLDER {
            continue;
        }
        spec.text = PROMPT.into();
        store.update_task_payload(&t.id, &spec.to_payload())?;
        migrated += 1;
    }
    Ok(migrated)
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

#[cfg(test)]
mod b13_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn store() -> (Store, std::path::PathBuf) {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ka-briefing-b13-{}-{}-{}",
            std::process::id(),
            ka_agent_core::now_ms(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (Store::open(&dir.join("state.db")).unwrap(), dir)
    }

    fn now() -> jiff::Zoned {
        "2026-10-30T10:00:00+01:00[Europe/Madrid]".parse().unwrap()
    }

    fn insert_briefing(s: &Store, text: &str) -> TaskId {
        let spec = CronSpec {
            text: text.into(),
            source: SOURCE.into(),
            recurrence_ms: Some(86_400_000),
            mode: CronMode::Agent,
            at_local: Some("08:00".into()),
            weekdays: Vec::new(),
            month_day: None,
            gate: None,
            role: None,
            username: None,
            tools: Vec::new(),
        };
        let mut task = Task::new("daily briefing".into(), "cron", false);
        task.payload = Some(spec.to_payload());
        s.insert_task(&task).unwrap();
        task.id
    }

    fn spec_of(s: &Store, id: &TaskId) -> CronSpec {
        let t = s.get_task(id).unwrap().unwrap();
        CronSpec::from_payload(t.payload.as_deref().unwrap()).unwrap()
    }

    /// `enable` stores the full prompt verbatim (the creation-time
    /// snapshot), keeping the task title short for lists.
    #[test]
    fn enable_bakes_the_full_prompt_into_the_payload() {
        let (s, dir) = store();
        let (id, _) = enable(&s, "08:00", &now()).unwrap();
        let spec = spec_of(&s, &id);
        assert_eq!(spec.text, PROMPT, "payload holds the full snapshot");
        assert_eq!(
            s.get_task(&id).unwrap().unwrap().title,
            "daily briefing",
            "title stays list-friendly"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The boot migration snapshots the current constant into legacy
    /// placeholder payloads, exactly once, and never touches anything
    /// else (other crons, or a briefing already holding a snapshot —
    /// e.g. taken under an older constant).
    #[test]
    fn snapshot_migration_rewrites_only_legacy_placeholders_once() {
        let (s, dir) = store();
        let legacy = insert_briefing(&s, "daily briefing");
        let snapshotted = insert_briefing(&s, "an older PROMPT constant's text");

        assert_eq!(snapshot_legacy_prompts(&s).unwrap(), 1);
        assert_eq!(spec_of(&s, &legacy).text, PROMPT);
        assert_eq!(
            spec_of(&s, &snapshotted).text,
            "an older PROMPT constant's text",
            "an existing snapshot is never rewritten"
        );
        assert_eq!(
            snapshot_legacy_prompts(&s).unwrap(),
            0,
            "self-terminating: the second boot is a no-op"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The fired text is the creation-time snapshot plus fire-time
    /// context — never the live constant (B13 immutability).
    #[test]
    fn fire_prompt_uses_the_snapshot_not_the_live_constant() {
        let spec = CronSpec {
            text: "OLD SNAPSHOT: reply exactly [SILENT] unless busy.".into(),
            source: SOURCE.into(),
            recurrence_ms: Some(86_400_000),
            mode: CronMode::Agent,
            at_local: Some("08:00".into()),
            weekdays: Vec::new(),
            month_day: None,
            gate: None,
            role: None,
            username: None,
            tools: Vec::new(),
        };
        let fired = fire_prompt(&spec, "today: 2 reminders due");
        assert!(fired.starts_with("OLD SNAPSHOT"), "fired: {fired}");
        assert!(
            fired.ends_with("\n\ntoday: 2 reminders due"),
            "fired: {fired}"
        );
        assert!(
            !fired.contains("Daily briefing for the operator"),
            "the live constant must not leak in: {fired}"
        );
    }
}
