//! Tasks: units of work tracked by Ka Agent with a persisted state machine
//! (SEED.md §2.10, §7.2). The scheduler arrives in Phase 3; the machine and
//! the restart rules are fixed now.

use serde::{Deserialize, Serialize};

use crate::ids::{SessionId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    WaitingApproval,
    Succeeded,
    Failed,
    Cancelled,
    /// Found `running` at startup after a crash.
    Interrupted,
    /// A side effect may or may not have happened; never auto-retried
    /// (SEED.md §7.3).
    Unknown,
    /// A scheduled fire that sat overdue beyond the late-grace window
    /// (SEED.md §7.2): recorded, reported, never fired.
    Missed,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Succeeded
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Interrupted
                | TaskStatus::Unknown
                | TaskStatus::Missed
        )
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use TaskStatus::*;
        matches!(
            (self, next),
            (Queued, Running)
                | (Queued, Cancelled)
                | (Running, WaitingApproval)
                | (WaitingApproval, Running)
                | (WaitingApproval, Cancelled)
                | (Running, Succeeded)
                | (Running, Failed)
                | (Running, Cancelled)
                | (Running, Interrupted)
                | (WaitingApproval, Interrupted)
                | (Running, Unknown)
                | (WaitingApproval, Unknown)
                // operator-driven reconciliation only
                | (Interrupted, Queued)
                | (Unknown, Queued)
                | (Interrupted, Cancelled)
                | (Unknown, Cancelled)
            | (Failed, Queued)
            | (Running, Missed)
        )
    }

    /// Restart reconciliation (SEED.md §7.2): `running` becomes interrupted;
    /// approvals for intents survive (the action never executed).
    pub fn on_restart(self) -> Self {
        match self {
            TaskStatus::Running => TaskStatus::Interrupted,
            other => other,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub session: Option<SessionId>,
    pub title: String,
    pub kind: String,
    pub status: TaskStatus,
    /// True for read-only kinds, which may be re-queued automatically once.
    pub read_only: bool,
    pub scheduled_at_ms: Option<i64>,
    /// JSON payload for kinds that need one (`kind = "cron"` stores its
    /// [`CronSpec`] here); NULL for every other kind.
    pub payload: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl Task {
    pub fn new(title: String, kind: &str, read_only: bool) -> Self {
        let now = crate::now_ms();
        Self {
            id: TaskId::generate(),
            session: None,
            title,
            kind: kind.to_string(),
            status: TaskStatus::Queued,
            read_only,
            scheduled_at_ms: None,
            payload: None,
            created_ms: now,
            updated_ms: now,
        }
    }

    pub fn transition(&mut self, next: TaskStatus) -> Result<(), TaskStatus> {
        if self.status.can_transition_to(next) {
            self.status = next;
            self.updated_ms = crate::now_ms();
            Ok(())
        } else {
            Err(next)
        }
    }
}

/// What a cron task runs when it fires (M11): a prompt for the agent, or a
/// no-agent handler registered by kind in the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronMode {
    /// The text is submitted to the engine as a prompt (a normal turn).
    Agent,
    /// The text is the argument for a runtime-registered handler (e.g.
    /// "calendar"): no engine, no LLM, deterministic output.
    NoAgent(String),
}

impl Serialize for CronMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(if matches!(self, CronMode::Agent) {
            1
        } else {
            2
        }))?;
        match self {
            CronMode::Agent => map.serialize_entry("mode", "agent")?,
            CronMode::NoAgent(kind) => {
                map.serialize_entry("mode", "no_agent")?;
                map.serialize_entry("kind", kind)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for CronMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = CronMode;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a cron mode object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<CronMode, A::Error> {
                let mut mode: Option<String> = None;
                let mut kind: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "mode" => mode = Some(map.next_value()?),
                        "kind" => kind = Some(map.next_value()?),
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                match mode.as_deref() {
                    Some("agent") => Ok(CronMode::Agent),
                    Some("no_agent") => Ok(CronMode::NoAgent(kind.unwrap_or_default())),
                    _ => Err(serde::de::Error::custom("missing or unknown mode")),
                }
            }
        }
        deserializer.deserialize_map(V)
    }
}

/// One generalized cron entry (M11), persisted as the `payload` of a
/// `kind = "cron"` task row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronSpec {
    /// The prompt (Agent) or the handler argument (NoAgent).
    pub text: String,
    /// Who created it: "operator", "kagent", ...
    pub source: String,
    /// Fixed interval between fires in ms; `None` = one-shot.
    pub recurrence_ms: Option<i64>,
    pub mode: CronMode,
    /// Anchored recurrence: the local wall-clock time ("HH:MM") each
    /// occurrence fires at, in the operator zone (DST-correct). When set,
    /// the next fire is computed from it instead of `recurrence_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_local: Option<String>,
    /// With `at_local`: only on these weekdays (0 = Monday); empty = every
    /// day.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weekdays: Vec<u8>,
    /// With `at_local`: this day of the month (1–31, clamped to month end).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub month_day: Option<u8>,
}

impl CronSpec {
    /// Serialize to the task-row payload (plain JSON, one line).
    ///
    /// Infallible for this plain struct (strings + integers); the scoped
    /// allow covers the workspace's `expect_used` gate.
    #[allow(clippy::expect_used)]
    pub fn to_payload(&self) -> String {
        serde_json::to_string(self).expect("CronSpec always serializes")
    }

    /// The days an anchored (`at_local`) recurrence fires on.
    pub fn day_rule(&self) -> crate::when::DayRule {
        use crate::when::DayRule;
        if let Some(d) = self.month_day {
            return DayRule::MonthDay(d);
        }
        let days: Vec<jiff::civil::Weekday> = self
            .weekdays
            .iter()
            .filter_map(|w| jiff::civil::Weekday::from_monday_zero_offset(*w as i8).ok())
            .collect();
        if days.is_empty() {
            DayRule::Daily
        } else {
            DayRule::Weekdays(days)
        }
    }

    /// Store a [`crate::when::DayRule`] in the spec's fields.
    pub fn set_day_rule(&mut self, rule: &crate::when::DayRule) {
        use crate::when::DayRule;
        self.weekdays.clear();
        self.month_day = None;
        match rule {
            DayRule::Daily => {}
            DayRule::Weekdays(days) => {
                self.weekdays = days
                    .iter()
                    .map(|d| d.to_monday_zero_offset() as u8)
                    .collect();
            }
            DayRule::MonthDay(d) => self.month_day = Some(*d),
        }
    }

    /// An agent cron from operator words — `in <dur> <text>`, `every <dur>
    /// <text>`, or `every <day rule> [at] <time> <text>` — with its first
    /// fire time. `Err` is the operator-facing reason (or the usage).
    pub fn from_words(
        words: &[&str],
        source: &str,
        now: &jiff::Zoned,
    ) -> Result<(Self, i64), String> {
        let usage = "usage: in <duration> <text> · every <duration> <text> · every day|weekday|mon,wed|month on the 1st at <time> <text>";
        let mut spec = Self {
            text: String::new(),
            source: source.to_string(),
            recurrence_ms: None,
            mode: CronMode::Agent,
            at_local: None,
            weekdays: Vec::new(),
            month_day: None,
        };
        let now_ms = now.timestamp().as_millisecond();
        let (first_at, used) = match words {
            ["in", dur, ..] => {
                let ms = crate::parse_duration(dur)
                    .ok_or_else(|| format!("bad duration {dur:?}; use Ns, Nm, Nh or Nd"))?;
                (now_ms + ms as i64, 2)
            }
            ["every", rest @ ..] => {
                if let Some((rule, (hh, mm), used)) = crate::when::parse_recurrence(rest) {
                    spec.recurrence_ms = Some(86_400_000);
                    spec.at_local = Some(format!("{hh:02}:{mm:02}"));
                    spec.set_day_rule(&rule);
                    (crate::when::next_local(hh, mm, &rule, now), 1 + used)
                } else {
                    let dur = rest.first().ok_or(usage)?;
                    let ms = crate::parse_duration(dur)
                        .ok_or_else(|| format!("bad duration {dur:?}; use Ns, Nm, Nh or Nd"))?;
                    spec.recurrence_ms = Some(ms as i64);
                    (now_ms + ms as i64, 2)
                }
            }
            _ => return Err(usage.into()),
        };
        spec.text = words.get(used..).unwrap_or_default().join(" ");
        if spec.text.trim().is_empty() {
            return Err(usage.into());
        }
        Ok((spec, first_at))
    }

    /// Parse a task-row payload; `None` on malformed or foreign JSON.
    pub fn from_payload(payload: &str) -> Option<Self> {
        serde_json::from_str(payload).ok()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn restart_reconciles_running_only() {
        assert_eq!(TaskStatus::Running.on_restart(), TaskStatus::Interrupted);
        assert_eq!(TaskStatus::Queued.on_restart(), TaskStatus::Queued);
        assert_eq!(
            TaskStatus::WaitingApproval.on_restart(),
            TaskStatus::WaitingApproval
        );
    }

    #[test]
    fn unknown_never_auto_reruns() {
        let mut t = Task::new("digest".into(), "rss", true);
        t.transition(TaskStatus::Running).unwrap();
        t.transition(TaskStatus::Unknown).unwrap();
        // only the operator can re-queue
        assert!(t.transition(TaskStatus::Running).is_err());
        t.transition(TaskStatus::Queued).unwrap();
    }
}

#[cfg(test)]
mod cron_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn cronspec_payload_roundtrip() {
        let one_shot = CronSpec {
            text: "say \"ping\" \\ now".into(),
            source: "kagent".into(),
            recurrence_ms: None,
            mode: CronMode::Agent,
            at_local: None,
            weekdays: Vec::new(),
            month_day: None,
        };
        assert_eq!(
            CronSpec::from_payload(&one_shot.to_payload()),
            Some(one_shot)
        );

        let recurring = CronSpec {
            text: "/home/op/cal.ics".into(),
            source: "operator".into(),
            recurrence_ms: Some(86_400_000),
            mode: CronMode::NoAgent("calendar".into()),
            at_local: Some("08:00".into()),
            weekdays: Vec::new(),
            month_day: None,
        };
        let payload = recurring.to_payload();
        assert!(payload.contains(r#""mode":"no_agent""#), "{payload}");
        assert!(payload.contains(r#""kind":"calendar""#), "{payload}");
        assert_eq!(CronSpec::from_payload(&payload), Some(recurring));
    }

    #[test]
    fn cronspec_payload_rejects_malformed() {
        assert_eq!(CronSpec::from_payload(""), None);
        assert_eq!(CronSpec::from_payload("not json"), None);
        assert_eq!(CronSpec::from_payload(r#"{"text":"x"}"#), None);
    }
}
