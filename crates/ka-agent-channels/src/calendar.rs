//! Calendar digest (M11): a no-agent cron handler. Reads an ICS source
//! (local path or granted URL) and renders today's events in the
//! operator's zone as plain text — no LLM, no engine.
//!
//! Times: `…Z` is UTC, `TZID=Area/City` is that zone, a bare time is
//! floating (the operator's zone); all are shown in the operator's zone.
//! Recurrence: `RRULE` FREQ=DAILY|WEEKLY|MONTHLY with INTERVAL, BYDAY
//! (weekly), COUNT and UNTIL is expanded inside the day window, `EXDATE`
//! honoured; anything else is listed once and noted.

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
    let tz = now.time_zone().clone();
    let day_start = now
        .date()
        .to_zoned(tz.clone())
        .unwrap_or_else(|_| now.clone());
    let day_end = now
        .date()
        .tomorrow()
        .ok()
        .and_then(|d| d.to_zoned(tz.clone()).ok())
        .unwrap_or_else(|| day_start.clone());
    let (events, unexpanded) = parse_events(raw, &tz);
    let mut rows: Vec<(i64, String)> = Vec::new();
    for ev in &events {
        for start in occurrences(ev, &day_start, &day_end) {
            let line = if ev.all_day {
                format!("(all day) {}", ev.summary)
            } else {
                let local = start.with_time_zone(tz.clone());
                let hhmm = |z: &jiff::Zoned| format!("{:02}:{:02}", z.hour(), z.minute());
                match ev.duration {
                    Some(d) => match local.checked_add(d) {
                        Ok(end) => format!("{}-{} {}", hhmm(&local), hhmm(&end), ev.summary),
                        Err(_) => format!("{} {}", hhmm(&local), ev.summary),
                    },
                    None => format!("{} {}", hhmm(&local), ev.summary),
                }
            };
            rows.push((start.timestamp().as_millisecond(), line));
        }
    }
    rows.sort_by_key(|(at, _)| *at);
    let mut out: Vec<String> = rows.into_iter().map(|(_, l)| l).collect();
    if out.is_empty() {
        out.push("no events today".to_string());
    }
    let mut text = out.join("\n");
    if unexpanded > 0 {
        text.push_str(&format!(
            "\n{unexpanded} recurring event(s) with rules this digest does not expand"
        ));
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

#[derive(Debug, Clone)]
struct Event {
    summary: String,
    /// First occurrence, in the event's own zone (DST-correct stepping).
    start: jiff::Zoned,
    all_day: bool,
    duration: Option<jiff::Span>,
    rule: Option<Rule>,
    exdates: Vec<jiff::Timestamp>,
}

/// The `RRULE` subset this digest expands.
#[derive(Debug, Clone)]
struct Rule {
    freq: Freq,
    interval: i64,
    by_day: Vec<jiff::civil::Weekday>,
    count: Option<usize>,
    until: Option<jiff::Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
}

/// Occurrence starts of `ev` inside `[from, to)`.
fn occurrences(ev: &Event, from: &jiff::Zoned, to: &jiff::Zoned) -> Vec<jiff::Zoned> {
    let (lo, hi) = (from.timestamp(), to.timestamp());
    let keep = |z: &jiff::Zoned| {
        let t = z.timestamp();
        // All-day events cover their whole date: overlap, not start.
        let inside = if ev.all_day {
            z.date() == from.date()
        } else {
            t >= lo && t < hi
        };
        inside && !ev.exdates.contains(&t)
    };
    let Some(rule) = &ev.rule else {
        return if keep(&ev.start) {
            vec![ev.start.clone()]
        } else {
            Vec::new()
        };
    };
    let mut out = Vec::new();
    let mut emitted = 0usize;
    let step = |k: i64| -> jiff::Span {
        match rule.freq {
            Freq::Daily => jiff::Span::new().days(k * rule.interval),
            Freq::Weekly => jiff::Span::new().weeks(k * rule.interval),
            Freq::Monthly => jiff::Span::new().months(k * rule.interval),
        }
    };
    // Periods from the first one; ten years of daily events is the cap.
    for k in 0..3_700i64 {
        let Ok(base) = ev.start.checked_add(step(k)) else {
            break;
        };
        // Weekly + BYDAY: every listed day of that week, in order.
        let mut candidates = if rule.freq == Freq::Weekly && !rule.by_day.is_empty() {
            let monday_offset = i64::from(base.weekday().to_monday_zero_offset());
            let mut days: Vec<jiff::Zoned> = rule
                .by_day
                .iter()
                .filter_map(|d| {
                    let delta = i64::from(d.to_monday_zero_offset()) - monday_offset;
                    base.checked_add(jiff::Span::new().days(delta)).ok()
                })
                .filter(|z| z.timestamp() >= ev.start.timestamp())
                .collect();
            days.sort_by_key(|z| z.timestamp());
            days
        } else {
            vec![base.clone()]
        };
        candidates.retain(|z| rule.until.is_none_or(|u| z.timestamp() <= u));
        if candidates.is_empty() && rule.until.is_some_and(|u| base.timestamp() > u) {
            break;
        }
        for z in candidates {
            if rule.count.is_some_and(|c| emitted >= c) {
                return out;
            }
            emitted += 1;
            if keep(&z) {
                out.push(z.clone());
            }
        }
        if base.timestamp() >= hi {
            break;
        }
    }
    out
}

/// Parse VEVENTs → (events, events whose rules are not expanded).
fn parse_events(raw: &str, tz: &jiff::tz::TimeZone) -> (Vec<Event>, usize) {
    let mut events = Vec::new();
    let mut unexpanded = 0usize;
    let mut in_event = false;
    let mut summary = String::new();
    let mut start: Option<(jiff::Zoned, bool)> = None;
    let mut end: Option<jiff::Zoned> = None;
    let mut rule: Option<Rule> = None;
    let mut exdates: Vec<jiff::Timestamp> = Vec::new();
    for line in unfold(raw) {
        let (name, params, value) = split_prop(&line);
        match (name.as_str(), in_event) {
            ("BEGIN", _) if value == "VEVENT" => {
                in_event = true;
                summary.clear();
                start = None;
                end = None;
                rule = None;
                exdates.clear();
            }
            ("END", true) if value == "VEVENT" => {
                if let Some((s, all_day)) = start.take() {
                    let duration = end
                        .take()
                        .and_then(|e| s.until(&e).ok())
                        .filter(|_| !all_day);
                    events.push(Event {
                        summary: std::mem::take(&mut summary),
                        start: s,
                        all_day,
                        duration,
                        rule: rule.take(),
                        exdates: std::mem::take(&mut exdates),
                    });
                }
                in_event = false;
            }
            (_, true) => match name.as_str() {
                "SUMMARY" => summary = value,
                "DTSTART" => start = parse_dt(&value, &params, tz),
                "DTEND" => end = parse_dt(&value, &params, tz).map(|(z, _)| z),
                "EXDATE" => exdates.extend(
                    value
                        .split(',')
                        .filter_map(|v| parse_dt(v, &params, tz))
                        .map(|(z, _)| z.timestamp()),
                ),
                "RRULE" => match parse_rule(&value, &params, tz) {
                    Some(r) => rule = Some(r),
                    None => unexpanded += 1,
                },
                _ => {}
            },
            _ => {}
        }
    }
    (events, unexpanded)
}

/// `FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;UNTIL=…;COUNT=…` (the subset above).
fn parse_rule(v: &str, params: &str, tz: &jiff::tz::TimeZone) -> Option<Rule> {
    let mut freq = None;
    let mut rule = Rule {
        freq: Freq::Daily,
        interval: 1,
        by_day: Vec::new(),
        count: None,
        until: None,
    };
    for part in v.split(';') {
        let (k, val) = part.split_once('=')?;
        match k.to_ascii_uppercase().as_str() {
            "FREQ" => {
                freq = Some(match val.to_ascii_uppercase().as_str() {
                    "DAILY" => Freq::Daily,
                    "WEEKLY" => Freq::Weekly,
                    "MONTHLY" => Freq::Monthly,
                    _ => return None,
                })
            }
            "INTERVAL" => rule.interval = val.parse().ok().filter(|n: &i64| *n >= 1)?,
            "COUNT" => rule.count = Some(val.parse().ok()?),
            "UNTIL" => rule.until = Some(parse_dt(val, params, tz)?.0.timestamp()),
            "BYDAY" => {
                for d in val.split(',') {
                    rule.by_day.push(match d.to_ascii_uppercase().as_str() {
                        "MO" => jiff::civil::Weekday::Monday,
                        "TU" => jiff::civil::Weekday::Tuesday,
                        "WE" => jiff::civil::Weekday::Wednesday,
                        "TH" => jiff::civil::Weekday::Thursday,
                        "FR" => jiff::civil::Weekday::Friday,
                        "SA" => jiff::civil::Weekday::Saturday,
                        "SU" => jiff::civil::Weekday::Sunday,
                        // `1MO`, `-1FR`: positional days are not expanded.
                        _ => return None,
                    });
                }
            }
            "WKST" => {}
            _ => return None,
        }
    }
    rule.freq = freq?;
    if !rule.by_day.is_empty() && rule.freq != Freq::Weekly {
        return None;
    }
    Some(rule)
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

/// `DTSTART;TZID=Europe/Madrid:20260120T090000` →
/// (`DTSTART`, `TZID=Europe/Madrid`, `20260120T090000`).
fn split_prop(line: &str) -> (String, String, String) {
    let Some((name_part, value)) = line.split_once(':') else {
        return (line.to_string(), String::new(), String::new());
    };
    let (name, params) = name_part.split_once(';').unwrap_or((name_part, ""));
    (
        name.trim().to_ascii_uppercase(),
        params.to_string(),
        value.to_string(),
    )
}

/// `YYYYMMDD` (all day), `YYYYMMDDTHHMMSSZ` (UTC), `…;TZID=Zone:` (that
/// zone) or a floating `YYYYMMDDTHHMMSS` (the operator's zone) → the
/// instant in its own zone, and whether it is all-day.
fn parse_dt(v: &str, params: &str, tz: &jiff::tz::TimeZone) -> Option<(jiff::Zoned, bool)> {
    let v = v.trim();
    let date = jiff::civil::Date::new(
        v.get(0..4)?.parse().ok()?,
        v.get(4..6)?.parse().ok()?,
        v.get(6..8)?.parse().ok()?,
    )
    .ok()?;
    let zone = params
        .split(';')
        .find_map(|p| p.strip_prefix("TZID="))
        .and_then(|name| jiff::tz::TimeZone::get(name.trim_matches('"')).ok())
        .unwrap_or_else(|| tz.clone());
    if v.len() == 8 {
        return Some((date.to_zoned(zone).ok()?, true));
    }
    if v.as_bytes().get(8) != Some(&b'T') {
        return None;
    }
    let time = jiff::civil::Time::new(
        v.get(9..11)?.parse().ok()?,
        v.get(11..13)?.parse().ok()?,
        v.get(13..15)?.parse().ok()?,
        0,
    )
    .ok()?;
    let dt = date.to_datetime(time);
    match &v[15..] {
        "Z" => Some((dt.to_zoned(jiff::tz::TimeZone::UTC).ok()?, false)),
        "" => Some((dt.to_zoned(zone).ok()?, false)),
        _ => None,
    }
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

    /// Wednesday 15 July 2026, 07:00 in Madrid (CEST, UTC+2).
    fn madrid_wed() -> jiff::Zoned {
        "2026-07-15T07:00:00+02:00[Europe/Madrid]".parse().unwrap()
    }

    fn cal(events: &str) -> String {
        format!("BEGIN:VCALENDAR\n{events}END:VCALENDAR\n")
    }

    fn ev(body: &str) -> String {
        format!("BEGIN:VEVENT\n{body}END:VEVENT\n")
    }

    #[test]
    fn utc_zone_and_floating_times_render_in_the_operator_zone() {
        let out = render(
            &cal(
                &(ev("SUMMARY:Zulu\nDTSTART:20260715T150000Z\nDTEND:20260715T153000Z\n")
                    + &ev("SUMMARY:NewYork\nDTSTART;TZID=America/New_York:20260715T090000\n")
                    + &ev("SUMMARY:Floating\nDTSTART:20260715T080000\n")),
            ),
            &madrid_wed(),
        );
        assert!(out.contains("17:00-17:30 Zulu"), "{out}");
        assert!(out.contains("15:00 NewYork"), "{out}");
        assert!(out.contains("08:00 Floating"), "{out}");
        // Sorted by start.
        let pos = |s: &str| out.find(s).unwrap();
        assert!(
            pos("Floating") < pos("NewYork") && pos("NewYork") < pos("Zulu"),
            "{out}"
        );
    }

    #[test]
    fn weekly_byday_rules_expand_and_exdates_are_skipped() {
        let weekly =
            ev("SUMMARY:Standup\nDTSTART:20260706T093000\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE\n");
        assert!(render(&cal(&weekly), &madrid_wed()).contains("09:30 Standup"));
        let thursday: jiff::Zoned = "2026-07-16T07:00:00+02:00[Europe/Madrid]".parse().unwrap();
        assert!(!render(&cal(&weekly), &thursday).contains("Standup"));
        let skipped = ev(
            "SUMMARY:Standup\nDTSTART:20260706T093000\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE\nEXDATE:20260715T093000\n",
        );
        assert!(!render(&cal(&skipped), &madrid_wed()).contains("Standup"));
        let fortnightly =
            ev("SUMMARY:Retro\nDTSTART:20260701T160000\nRRULE:FREQ=WEEKLY;INTERVAL=2\n");
        assert!(render(&cal(&fortnightly), &madrid_wed()).contains("16:00 Retro"));
        let off_week = ev("SUMMARY:Retro\nDTSTART:20260708T160000\nRRULE:FREQ=WEEKLY;INTERVAL=2\n");
        assert!(!render(&cal(&off_week), &madrid_wed()).contains("Retro"));
    }

    #[test]
    fn daily_count_until_and_monthly_rules() {
        let three = ev("SUMMARY:Course\nDTSTART:20260713T180000\nRRULE:FREQ=DAILY;COUNT=3\n");
        assert!(
            render(&cal(&three), &madrid_wed()).contains("18:00 Course"),
            "third of three"
        );
        let thursday: jiff::Zoned = "2026-07-16T07:00:00+02:00[Europe/Madrid]".parse().unwrap();
        assert!(
            !render(&cal(&three), &thursday).contains("Course"),
            "COUNT exhausted"
        );
        let until = ev(
            "SUMMARY:Sprint\nDTSTART:20260710T100000\nRRULE:FREQ=DAILY;UNTIL=20260714T235959Z\n",
        );
        assert!(!render(&cal(&until), &madrid_wed()).contains("Sprint"));
        let rent = ev("SUMMARY:Rent\nDTSTART;VALUE=DATE:20260115\nRRULE:FREQ=MONTHLY\n");
        assert!(render(&cal(&rent), &madrid_wed()).contains("(all day) Rent"));
    }

    #[test]
    fn unsupported_rules_are_noted_and_the_rest_still_renders() {
        let out = render(
            &cal(&(ev(
                "SUMMARY:FirstMonday\nDTSTART:20260706T100000\nRRULE:FREQ=MONTHLY;BYDAY=1MO\n",
            ) + &ev("SUMMARY:Folded\n PartTwo\nDTSTART:20260715T120000\n"))),
            &madrid_wed(),
        );
        assert!(out.contains("12:00 FoldedPartTwo"), "{out}");
        assert!(
            out.contains("1 recurring event(s) with rules this digest does not expand"),
            "{out}"
        );
    }

    #[test]
    fn empty_day_says_so() {
        let out = render(
            &cal(&ev("SUMMARY:Far\nDTSTART:20260720T100000Z\n")),
            &madrid_wed(),
        );
        assert_eq!(out, "no events today");
    }

    #[test]
    fn local_path_source_is_read() {
        let root = temp_root("path");
        let store = store_at(&root.join("data"));
        let today = jiff::Zoned::now().with_time_zone(jiff::tz::TimeZone::UTC);
        let stamp = format!("{}T120000Z", today.strftime("%Y%m%d"));
        let ics = root.join("cal.ics");
        std::fs::write(
            &ics,
            cal(&ev(&format!("SUMMARY:FromFile\nDTSTART:{stamp}\n"))),
        )
        .unwrap();
        let out = digest_in(ics.to_str().unwrap(), &store, &jiff::tz::TimeZone::UTC).unwrap();
        assert!(out.contains("12:00 FromFile"), "{out}");
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
