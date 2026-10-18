//! Calendar digest (M11): a no-agent cron handler. Reads an ICS source
//! (local path or granted URL) and renders the events of the next 24 h as
//! plain text — no LLM, no engine.
//!
//! Minimal by design (documented in `docs/contract/cron-and-refs.md`):
//! no `RRULE` expansion (recurring events are counted and noted), and times
//! are shown exactly as the ICS states them (`Z` suffix kept, no timezone
//! conversion).

use ka_agent_store::Store;

use crate::grants;
use crate::mcp;

/// Render the digest for one ICS source now. `Ok(text)` is always a
/// presentable answer; `Err` is a visible failure (bad source, refused
/// fetch).
pub fn digest(source: &str, store: &Store) -> Result<String, String> {
    digest_in(source, store, &jiff::tz::TimeZone::system())
}

/// Digest in the operator's zone: the window is the local day
/// (`[today 00:00, tomorrow 00:00)`), and wall-clock times in the ICS that
/// carry a `Z` are labelled with the operator's offset, not left as `Z`.
pub fn digest_in(source: &str, store: &Store, tz: &jiff::tz::TimeZone) -> Result<String, String> {
    let raw = load(source, store)?;
    let now = jiff::Timestamp::now().to_zoned(tz.clone());
    Ok(render(&raw, &now))
}

/// ICS body → digest lines for the local `[today 00:00, tomorrow 00:00)`
/// window, times in `now`'s zone.
fn render(raw: &str, now: &jiff::Zoned) -> String {
    let day_start = now
        .with()
        .date(now.date())
        .time(jiff::civil::time(0, 0, 0, 0))
        .build()
        .unwrap_or_else(|_| {
            now.with()
                .time(jiff::civil::time(0, 0, 0, 0))
                .build()
                .unwrap_or_else(|_| now.clone())
        });
    let window_start = day_start.timestamp().as_millisecond();
    let window_end = window_start + 86_400_000;
    let (mut events, recurring) = parse_events(raw);
    events.retain(|e| e.start_ms >= window_start && e.start_ms < window_end);
    events.sort_by_key(|e| e.start_ms);
    let mut out: Vec<String> = events
        .iter()
        .map(|ev| match (&ev.start, &ev.end) {
            (Some(s), Some(e)) => format!("{}-{} {}", label(s, now), label(e, now), ev.summary),
            (Some(s), None) => format!("{} {}", label(s, now), ev.summary),
            (None, _) => format!("(all day) {}", ev.summary),
        })
        .collect();
    if out.is_empty() {
        out.push("no events in the next 24h".to_string());
    }
    let mut text = out.join("\n");
    if recurring > 0 {
        text.push_str(&format!("\n{recurring} recurring event(s) not expanded"));
    }
    text
}

/// Load the ICS body: granted+SSRF-guarded HTTP(S) or a local file path.
fn load(source: &str, store: &Store) -> Result<String, String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let parsed = url::Url::parse(source).map_err(|e| format!("bad url: {e}"))?;
        let host = parsed.host_str().unwrap_or("").to_string();
        let g = grants::Grants::load(store).map_err(|e| format!("grants: {e}"))?;
        let scope = format!("http:get:{host}");
        if !g.check(&scope) {
            return Err(format!("no grant for {scope}"));
        }
        if !g.check("http:get:private") && mcp::http_target_is_private(&host) {
            return Err("private target needs http:get:private".into());
        }
        mcp::http_get_no_redirect(&parsed, 15, &g)
    } else {
        std::fs::read_to_string(source).map_err(|e| format!("read {source}: {e}"))
    }
}

/// Displayed time inside a digest line.
#[derive(Debug, Clone)]
struct Time {
    /// `HH:MM` as stated in the ICS (no zone conversion).
    hhmm: String,
    /// The ICS carried a trailing `Z`.
    zulu: bool,
}

/// The event's wall-clock time. A `Z` time is converted into `now`'s zone
/// (a local time keeps its zone's wall clock as stated).
fn label(t: &Time, now: &jiff::Zoned) -> String {
    if !t.zulu {
        return t.hhmm.clone();
    }
    let Some((h, m)) = t.hhmm.split_once(':') else {
        return format!("{}Z", t.hhmm);
    };
    let (h, m): (i8, i8) = (h.parse().unwrap_or(0), m.parse().unwrap_or(0));
    let Ok(at) = now
        .with()
        .hour(h.clamp(0, 23))
        .minute(m.clamp(0, 59))
        .build()
    else {
        return format!("{}Z", t.hhmm);
    };
    format!("{:02}:{:02}", at.hour(), at.minute())
}

#[derive(Debug, Clone)]
struct Event {
    summary: String,
    start_ms: i64,
    /// `None` for DATE-only (all-day) starts.
    start: Option<Time>,
    end: Option<Time>,
}

/// Parse VEVENTs → (events, recurring RRULE count). Line folding
/// (continuation lines start with a space/tab) is unfolded first.
fn parse_events(raw: &str) -> (Vec<Event>, usize) {
    let mut events = Vec::new();
    let mut recurring = 0usize;
    let mut in_event = false;
    let mut summary = String::new();
    let mut start: Option<(i64, Option<Time>)> = None;
    let mut end: Option<Time> = None;
    for line in unfold(raw) {
        let (name, value) = split_prop(&line);
        match (name.as_str(), in_event) {
            ("BEGIN", _) if value == "VEVENT" => {
                in_event = true;
                summary.clear();
                start = None;
                end = None;
            }
            ("END", true) if value == "VEVENT" => {
                if let Some((start_ms, start_time)) = start.take() {
                    events.push(Event {
                        summary: std::mem::take(&mut summary),
                        start_ms,
                        start: start_time,
                        end: end.take(),
                    });
                }
                in_event = false;
            }
            (_, true) => match name.as_str() {
                "SUMMARY" => summary = value,
                "DTSTART" => start = parse_dt(&value),
                "DTEND" => end = parse_dt(&value).and_then(|(_, t)| t),
                "RRULE" => recurring += 1,
                _ => {}
            },
            _ => {}
        }
    }
    (events, recurring)
}

/// RFC 5545 §3.1: a CRLF followed by one WSP is a fold — join by dropping
/// the WSP.
fn unfold(raw: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if (line.starts_with(' ') || line.starts_with('\t')) && !lines.is_empty() {
            if let Some(last) = lines.last_mut() {
                last.push_str(&line[1..]);
            }
        } else {
            lines.push(line.to_string());
        }
    }
    lines
}

/// `DTSTART;VALUE=DATE:20260120` → (`DTSTART`, `20260120`).
fn split_prop(line: &str) -> (String, String) {
    let Some((name_part, value)) = line.split_once(':') else {
        return (line.to_string(), String::new());
    };
    let name = name_part
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    (name, value.to_string())
}

/// `YYYYMMDD[T?HHMMSS[Z]]` → (epoch ms, display time). DATE-only values
/// yield `None` time (all-day).
fn parse_dt(v: &str) -> Option<(i64, Option<Time>)> {
    let v = v.trim();
    if v.len() != 8 && v.len() != 15 && v.len() != 16 {
        return None;
    }
    let y: i64 = v.get(0..4)?.parse().ok()?;
    let m: i64 = v.get(4..6)?.parse().ok()?;
    let d: i64 = v.get(6..8)?.parse().ok()?;
    let days = days_from_civil(y, m, d);
    if v.len() == 8 {
        return Some((days * 86_400_000, None));
    }
    if v.as_bytes().get(8) != Some(&b'T') {
        return None;
    }
    let hh: i64 = v.get(9..11)?.parse().ok()?;
    let mi: i64 = v.get(11..13)?.parse().ok()?;
    let _ss: i64 = v.get(13..15)?.parse().ok()?;
    let zulu = v.len() == 16;
    if !zulu && v.len() == 16 {
        return None;
    }
    if zulu && !v.ends_with('Z') {
        return None;
    }
    let secs = days * 86_400 + hh * 3_600 + mi * 60;
    Some((
        secs * 1_000,
        Some(Time {
            hhmm: format!("{hh:02}:{mi:02}"),
            zulu,
        }),
    ))
}

/// Civil date → days since the Unix epoch (Howard Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-cal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store_at(dir: &std::path::Path) -> Store {
        Store::open(&dir.join("state.db")).unwrap()
    }

    /// An event time inside the digest window (the local day): half an
    /// hour out, clamped to 23:58 so a test running near midnight stays
    /// inside today.
    fn day_safe(now_ms: i64) -> i64 {
        let now = jiff::Timestamp::from_millisecond(now_ms)
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::system());
        let end = now
            .with()
            .time(jiff::civil::time(23, 58, 0, 0))
            .build()
            .unwrap();
        let candidate = now + jiff::Span::new().minutes(30);
        if candidate <= end {
            candidate.timestamp().as_millisecond()
        } else {
            end.timestamp().as_millisecond()
        }
    }

    /// `YYYYMMDDTHHMMSSZ` for `now + offset_ms`, and its `HH:MM`.
    fn stamp(now: i64, offset_ms: i64) -> (String, String) {
        let t = now + offset_ms;
        let secs = t.div_euclid(1000);
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (h, m) = (rem / 3600, (rem % 3600) / 60);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if month <= 2 { y + 1 } else { y };
        (
            format!("{year:04}{month:02}{d:02}T{h:02}{m:02}00Z"),
            format!("{h:02}:{m:02}"),
        )
    }

    #[test]
    fn event_inside_window_is_listed_in_operator_zone() {
        let now_ms = ka_agent_core::now_ms();
        let now = jiff::Timestamp::from_millisecond(now_ms)
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::system());
        let (start, hhmm) = stamp(day_safe(now_ms), 0);
        let (end, _) = stamp(day_safe(now_ms) + 1_800_000, 0);
        let ics = format!(
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:CTest-XYZ\nDTSTART:{start}\nDTEND:{end}\nEND:VEVENT\nEND:VCALENDAR\n"
        );
        let out = render(&ics, &now);
        // The ICS time is `Z`; the digest labels it in the operator zone.
        assert!(!out.contains(&format!("{hhmm}Z")), "no zulu suffix: {out}");
        assert!(out.contains(&hhmm), "{out}");
        assert!(out.contains("CTest-XYZ"), "{out}");
    }

    #[test]
    fn event_outside_window_is_not_listed() {
        let now_ms = ka_agent_core::now_ms();
        let now = jiff::Timestamp::from_millisecond(now_ms)
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::system());
        let (start, _) = stamp(now_ms + 3 * 86_400_000, 0); // +3 days: outside today
        let ics = format!(
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Far\nDTSTART:{start}\nEND:VEVENT\nEND:VCALENDAR\n"
        );
        let out = render(&ics, &now);
        assert!(out.contains("no events in the next 24h"), "{out}");
    }

    #[test]
    fn date_only_event_is_all_day() {
        let now_ms = ka_agent_core::now_ms();
        let now = jiff::Timestamp::from_millisecond(now_ms)
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::system());
        // Next UTC midnight (or `now` itself at an exact boundary): always
        // inside the local day window.
        // Noon of the local day: always inside [today 00:00, tomorrow 00:00).
        let noon = now
            .with()
            .time(jiff::civil::time(12, 0, 0, 0))
            .build()
            .unwrap()
            .timestamp()
            .as_millisecond();
        let (stamp_str, _) = stamp(noon, 0);
        let date_only = &stamp_str[..8];
        let ics = format!(
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:WholeDay\nDTSTART;VALUE=DATE:{date_only}\nEND:VEVENT\nEND:VCALENDAR\n"
        );
        let out = render(&ics, &now);
        assert!(out.contains("(all day) WholeDay"), "{out}");
    }

    #[test]
    fn folded_summary_lines_are_unfolded() {
        let now = jiff::Timestamp::from_millisecond(ka_agent_core::now_ms())
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::system());
        let (start, _) = stamp(day_safe(ka_agent_core::now_ms()), 0);
        let ics = format!(
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Folded\n PartTwo\nDTSTART:{start}\nEND:VEVENT\nEND:VCALENDAR\n"
        );
        let out = render(&ics, &now);
        assert!(out.contains("FoldedPartTwo"), "{out}");
    }

    #[test]
    fn rrule_events_are_counted_not_expanded() {
        let now = jiff::Timestamp::from_millisecond(ka_agent_core::now_ms())
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::system());
        let (start, _) = stamp(day_safe(ka_agent_core::now_ms()), 0);
        let ics = format!(
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Daily\nDTSTART:{start}\nRRULE:FREQ=DAILY\nEND:VEVENT\nEND:VCALENDAR\n"
        );
        let out = render(&ics, &now);
        assert!(out.contains("Daily"), "{out}");
        assert!(out.contains("1 recurring event(s) not expanded"), "{out}");
    }

    #[test]
    fn events_are_sorted_by_start() {
        let now = jiff::Timestamp::from_millisecond(ka_agent_core::now_ms())
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::system());
        // Both inside today: the clamped slot and half an hour before it.
        let safe = day_safe(ka_agent_core::now_ms());
        let (late, _) = stamp(safe, 0);
        let (early, _) = stamp(safe - 1_800_000, 0);
        let ics = format!(
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Late\nDTSTART:{late}\nEND:VEVENT\nBEGIN:VEVENT\nSUMMARY:Early\nDTSTART:{early}\nEND:VEVENT\nEND:VCALENDAR\n"
        );
        let out = render(&ics, &now);
        let early_at = out.find("Early").expect("Early listed");
        let late_at = out.find("Late").expect("Late listed");
        assert!(early_at < late_at, "{out}");
    }

    #[test]
    fn local_path_source_is_read() {
        let root = temp_root("path");
        let store = store_at(&root.join("data"));
        let _now = jiff::Timestamp::from_millisecond(ka_agent_core::now_ms())
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::system());
        let (start, _) = stamp(day_safe(ka_agent_core::now_ms()), 0);
        let ics = root.join("cal.ics");
        std::fs::write(
            &ics,
            format!(
                "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:FromFile\nDTSTART:{start}\nEND:VEVENT\nEND:VCALENDAR\n"
            ),
        )
        .unwrap();
        let out = digest(ics.to_str().unwrap(), &store).unwrap();
        assert!(out.contains("FromFile"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_path_source_is_a_visible_error() {
        let root = temp_root("missing-path");
        let store = store_at(&root.join("data"));
        let err = digest("/nonexistent/cal.ics", &store).unwrap_err();
        assert!(err.contains("read /nonexistent/cal.ics"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
