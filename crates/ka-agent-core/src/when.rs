//! Local-time grammar for scheduling (`/remind`, `/cron … at`, calendar).
//!
//! Every function is pure: the caller supplies `now` as a [`jiff::Zoned`] in
//! the operator's zone, and all clock times are interpreted in that zone
//! (DST-correct via jiff's "compatible" disambiguation: a clock time inside a
//! spring-forward gap moves forward, an ambiguous fall-back time takes the
//! earlier instant).

use jiff::civil::{Date, DateTime, Time, Weekday};
use jiff::{Timestamp, Zoned};

use crate::time::MAX_DURATION_MS;

const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;

/// A parsed scheduling expression: the absolute instant and how many leading
/// whitespace tokens it used (the rest is the reminder text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct When {
    pub at_ms: i64,
    pub consumed: usize,
}

/// A clock token: `(hour, minute, explicit am/pm, had a colon)`.
struct Clock {
    hh: u8,
    mm: u8,
    meridiem: bool,
    colon: bool,
}

fn parse_clock(s: &str) -> Option<Clock> {
    let s = s.trim().to_ascii_lowercase();
    let (body, pm) = if let Some(b) = s.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = s.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (s.as_str(), None)
    };
    let (h, m, colon) = match body.split_once(':') {
        Some((h, m)) => (h, Some(m), true),
        None => (body, None, false),
    };
    if h.is_empty() || h.len() > 2 || !h.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut hh: u8 = h.parse().ok()?;
    let mm: u8 = match m {
        Some(m) if m.len() == 2 && m.bytes().all(|b| b.is_ascii_digit()) => m.parse().ok()?,
        Some(_) => return None,
        None => 0,
    };
    if mm > 59 {
        return None;
    }
    match pm {
        Some(pm) => {
            if !(1..=12).contains(&hh) {
                return None;
            }
            hh = match (hh, pm) {
                (12, false) => 0,
                (12, true) => 12,
                (h, true) => h + 12,
                (h, false) => h,
            };
        }
        None if hh > 23 => return None,
        None => {}
    }
    Some(Clock {
        hh,
        mm,
        meridiem: pm.is_some(),
        colon,
    })
}

/// Parse a clock time: `9`, `09:30`, `9am`, `9:15pm`, `21:00`
/// (case-insensitive). Returns `(hour 0..=23, minute 0..=59)`.
pub fn parse_hhmm(s: &str) -> Option<(u8, u8)> {
    parse_clock(s).map(|c| (c.hh, c.mm))
}

/// A compound duration `([0-9]+[dhms])+` (`2h30m`, `90m`, `1d12h`) in
/// milliseconds; zero and anything above [`MAX_DURATION_MS`] are refused.
fn parse_compound_duration(s: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut digits = String::new();
    let mut groups = 0usize;
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let mult: u64 = match c {
            'd' => 86_400_000,
            'h' => 3_600_000,
            'm' => 60_000,
            's' => 1_000,
            _ => return None,
        };
        if digits.is_empty() {
            return None;
        }
        let n: u64 = digits.parse().ok()?;
        total = total.checked_add(n.checked_mul(mult)?)?;
        digits.clear();
        groups += 1;
    }
    if !digits.is_empty() || groups == 0 || total == 0 || total > MAX_DURATION_MS {
        return None;
    }
    Some(total)
}

/// A weekday name or abbreviation (`mon`, `tues`, `friday`), lower-case.
pub fn parse_weekday(s: &str) -> Option<Weekday> {
    Some(match s {
        "mon" | "monday" => Weekday::Monday,
        "tue" | "tues" | "tuesday" => Weekday::Tuesday,
        "wed" | "wednesday" => Weekday::Wednesday,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thursday,
        "fri" | "friday" => Weekday::Friday,
        "sat" | "saturday" => Weekday::Saturday,
        "sun" | "sunday" => Weekday::Sunday,
        _ => return None,
    })
}

/// `YYYY-MM-DD` exactly (no other ISO shapes).
fn parse_date(s: &str) -> Option<Date> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<&str> {
        let part = s.get(r)?;
        part.bytes().all(|c| c.is_ascii_digit()).then_some(part)
    };
    let y: i16 = digits(0..4)?.parse().ok()?;
    let m: i8 = digits(5..7)?.parse().ok()?;
    let d: i8 = digits(8..10)?.parse().ok()?;
    Date::new(y, m, d).ok()
}

/// The instant of `date hh:mm` in `now`'s zone.
fn local_ms(date: Date, hh: u8, mm: u8, now: &Zoned) -> Option<i64> {
    let time = Time::new(i8::try_from(hh).ok()?, i8::try_from(mm).ok()?, 0, 0).ok()?;
    let zoned = DateTime::from_parts(date, time)
        .to_zoned(now.time_zone().clone())
        .ok()?;
    Some(zoned.timestamp().as_millisecond())
}

fn add_days(date: Date, days: u8) -> Option<Date> {
    let mut d = date;
    for _ in 0..days {
        d = d.tomorrow().ok()?;
    }
    Some(d)
}

fn now_ms(now: &Zoned) -> i64 {
    now.timestamp().as_millisecond()
}

/// Strictly in the future and within [`MAX_DURATION_MS`] of now.
fn in_window(at_ms: i64, now: &Zoned) -> Option<i64> {
    let delta = at_ms.checked_sub(now_ms(now))?;
    (delta > 0 && delta as u64 <= MAX_DURATION_MS).then_some(at_ms)
}

enum Day {
    Today,
    Tonight,
    Tomorrow,
    Weekday(Weekday),
}

fn parse_day(s: &str) -> Option<Day> {
    match s {
        "today" => Some(Day::Today),
        "tonight" => Some(Day::Tonight),
        "tomorrow" | "tmrw" => Some(Day::Tomorrow),
        other => parse_weekday(other).map(Day::Weekday),
    }
}

fn day_at(day: &Day, clock: Option<&Clock>, now: &Zoned) -> Option<i64> {
    let today = now.date();
    let (mut hh, mm) = match (clock, day) {
        (Some(c), _) => (c.hh, c.mm),
        (None, Day::Tonight) => (20, 0),
        (None, _) => (9, 0),
    };
    // `tonight 9` means 21:00 unless the operator said `am`.
    if let (Day::Tonight, Some(c)) = (day, clock)
        && !c.meridiem
        && hh < 12
    {
        hh += 12;
    }
    let at = match day {
        Day::Today | Day::Tonight => local_ms(today, hh, mm, now)?,
        Day::Tomorrow => local_ms(add_days(today, 1)?, hh, mm, now)?,
        Day::Weekday(wd) => {
            let ahead = (wd.to_monday_zero_offset() - today.weekday().to_monday_zero_offset())
                .rem_euclid(7) as u8;
            let at = local_ms(add_days(today, ahead)?, hh, mm, now)?;
            if at > now_ms(now) {
                at
            } else {
                local_ms(add_days(today, ahead + 7)?, hh, mm, now)?
            }
        }
    };
    in_window(at, now)
}

/// Try to read exactly `tokens` (already lower-cased) as one expression.
fn parse_exact(tokens: &[String], now: &Zoned) -> Option<i64> {
    let t: Vec<&str> = tokens.iter().map(String::as_str).collect();
    match t.as_slice() {
        // Relative: `[in] 2h30m`.
        ["in", d] => relative(d, now),
        // Clock: `at 18:30`.
        ["at", c] => clock_today_or_tomorrow(&parse_clock(c)?, now),
        [one] => {
            if let Some(at) = relative(one, now) {
                return Some(at);
            }
            // A bare hour (`9`) needs `at` or a day so a leading number in
            // the reminder text is not read as a time.
            if let Some(clock) = parse_clock(one) {
                return (clock.meridiem || clock.colon)
                    .then(|| clock_today_or_tomorrow(&clock, now))
                    .flatten();
            }
            day_or_date(one, None, now)
        }
        [head, c] | [head, "at", c] => day_or_date(head, Some(parse_clock(c)?), now),
        _ => None,
    }
}

fn relative(token: &str, now: &Zoned) -> Option<i64> {
    let ms = i64::try_from(parse_compound_duration(token)?).ok()?;
    now_ms(now).checked_add(ms)
}

/// Day (+ clock): `tomorrow`, `fri 17:00`, `tonight at 9`; or a date
/// `2026-11-03 [clock]` (default 09:00).
fn day_or_date(head: &str, clock: Option<Clock>, now: &Zoned) -> Option<i64> {
    if let Some(day) = parse_day(head) {
        return day_at(&day, clock.as_ref(), now);
    }
    let date = parse_date(head)?;
    let (hh, mm) = clock.map_or((9, 0), |c| (c.hh, c.mm));
    in_window(local_ms(date, hh, mm, now)?, now)
}

fn clock_today_or_tomorrow(clock: &Clock, now: &Zoned) -> Option<i64> {
    let today = now.date();
    let at = local_ms(today, clock.hh, clock.mm, now)?;
    if at > now_ms(now) {
        return Some(at);
    }
    local_ms(add_days(today, 1)?, clock.hh, clock.mm, now)
}

/// Parse a scheduling expression from the start of `tokens`, longest match
/// first (3, then 2, then 1 tokens). Forms, all in `now`'s zone:
///
/// - relative: `[in] 2h30m` (compound `([0-9]+[dhms])+`, ≤ 366 days);
/// - clock: `[at] 18:30` / `9am` — today if still ahead, else tomorrow
///   (a bare hour like `9` needs `at`);
/// - day + clock: `today|tonight|tomorrow|mon…sunday [at] <clock>`; a bare
///   day means 09:00 (`tonight` 20:00); a weekday equal to today means today
///   if the time is still ahead, otherwise next week;
/// - date: `YYYY-MM-DD [[at] <clock>]` (default 09:00).
///
/// Absolute times in the past (or more than 366 days ahead) are `None`.
pub fn parse_when(tokens: &[&str], now: &Zoned) -> Option<When> {
    let lower: Vec<String> = tokens
        .iter()
        .take(3)
        .map(|t| t.to_ascii_lowercase())
        .collect();
    (1..=lower.len())
        .rev()
        .find_map(|n| parse_exact(&lower[..n], now).map(|at_ms| When { at_ms, consumed: n }))
}

/// Which days an anchored recurrence fires on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DayRule {
    Daily,
    /// These weekdays (any order, no duplicates needed).
    Weekdays(Vec<Weekday>),
    /// This day of the month (1–31); months that are shorter fire on their
    /// last day, so "the 31st" means "month end".
    MonthDay(u8),
}

impl DayRule {
    fn matches(&self, date: Date) -> bool {
        match self {
            Self::Daily => true,
            Self::Weekdays(days) => days.contains(&date.weekday()),
            Self::MonthDay(d) => {
                let want = (*d).clamp(1, 31) as i8;
                let last = date.days_in_month();
                date.day() == want.min(last)
            }
        }
    }

    /// Operator wording: `daily`, `weekdays`, `mon,wed,fri`, `monthly on the 1st`.
    pub fn describe(&self) -> String {
        const NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
        match self {
            Self::Daily => "daily".into(),
            Self::Weekdays(days) => {
                let mut idx: Vec<usize> = days
                    .iter()
                    .map(|d| d.to_monday_zero_offset().clamp(0, 6) as usize)
                    .collect();
                idx.sort_unstable();
                idx.dedup();
                match idx.as_slice() {
                    [0, 1, 2, 3, 4] => "weekdays".into(),
                    [5, 6] => "weekends".into(),
                    _ => idx.iter().map(|i| NAMES[*i]).collect::<Vec<_>>().join(","),
                }
            }
            Self::MonthDay(d) => format!("monthly on the {}", ordinal(*d)),
        }
    }
}

fn ordinal(n: u8) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// The next instant strictly after `now` whose local clock reads `hh:mm`
/// on a day `rule` allows, DST-correct. Out-of-range clock values are
/// clamped to 23:59.
pub fn next_local(hh: u8, mm: u8, rule: &DayRule, now: &Zoned) -> i64 {
    let (hh, mm) = (hh.min(23), mm.min(59));
    let after = now_ms(now);
    let mut date = now.date();
    // Two months cover every rule (a month day, including "today but
    // already past").
    for _ in 0..=62 {
        if rule.matches(date)
            && let Some(at) = local_ms(date, hh, mm, now)
            && at > after
        {
            return at;
        }
        match date.tomorrow() {
            Ok(next) => date = next,
            Err(_) => break,
        }
    }
    // Only reachable at the end of jiff's supported range.
    after.saturating_add(DAY_MS)
}

/// A recurrence after the word `every`: the day rule, the clock (hh, mm)
/// and how many tokens it used. Forms (case-insensitive, `at` optional):
///
/// - `day at 08:00`
/// - `weekday at 9` · `weekend at 10am`
/// - `mon at 9` · `mon,wed,fri at 18:00`
/// - `month on the 1st at 9` · `month on 15 at 09:30`
pub fn parse_recurrence(tokens: &[&str]) -> Option<(DayRule, (u8, u8), usize)> {
    let lower: Vec<String> = tokens.iter().map(|t| t.to_ascii_lowercase()).collect();
    let first = lower.first()?;
    let (rule, mut used) = match first.as_str() {
        "day" | "daily" => (DayRule::Daily, 1),
        "weekday" | "weekdays" => (
            DayRule::Weekdays(vec![
                Weekday::Monday,
                Weekday::Tuesday,
                Weekday::Wednesday,
                Weekday::Thursday,
                Weekday::Friday,
            ]),
            1,
        ),
        "weekend" | "weekends" => (
            DayRule::Weekdays(vec![Weekday::Saturday, Weekday::Sunday]),
            1,
        ),
        "month" => {
            let mut i = 1;
            if lower.get(i).is_some_and(|t| t == "on") {
                i += 1;
            }
            if lower.get(i).is_some_and(|t| t == "the") {
                i += 1;
            }
            let raw = lower.get(i)?;
            let digits: String = raw.chars().take_while(char::is_ascii_digit).collect();
            let rest = &raw[digits.len()..];
            if !matches!(rest, "" | "st" | "nd" | "rd" | "th") {
                return None;
            }
            let day: u8 = digits.parse().ok().filter(|d| (1..=31).contains(d))?;
            (DayRule::MonthDay(day), i + 1)
        }
        list => {
            let days: Option<Vec<Weekday>> = list.split(',').map(parse_weekday).collect();
            let days = days.filter(|d| !d.is_empty())?;
            (DayRule::Weekdays(days), 1)
        }
    };
    if lower.get(used).is_some_and(|t| t == "at") {
        used += 1;
    }
    let clock = parse_hhmm(lower.get(used)?)?;
    Some((rule, clock, used + 1))
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Render an instant for the operator in `now`'s zone:
/// `today 18:30 (in 2h 5m)`, `tomorrow 09:00 (in 14h)`,
/// `Mon 20 Oct 09:00 (in 3d)`. Past instants read `today 08:00 (5m ago)` /
/// `yesterday 21:00 (13h ago)`; within 30 s of now the part is `(now)`.
/// Other years append the year (`Mon 20 Oct 2027 09:00`).
pub fn describe(at_ms: i64, now: &Zoned) -> String {
    let Ok(ts) = Timestamp::from_millisecond(at_ms) else {
        return format!("@{at_ms}ms");
    };
    let at = ts.to_zoned(now.time_zone().clone());
    let (day, today) = (at.date(), now.date());
    let label = if day == today {
        "today".to_string()
    } else if today.tomorrow().ok() == Some(day) {
        "tomorrow".to_string()
    } else if today.yesterday().ok() == Some(day) {
        "yesterday".to_string()
    } else {
        let wd = WEEKDAYS[day.weekday().to_monday_zero_offset().clamp(0, 6) as usize];
        let mon = MONTHS[(day.month() - 1).clamp(0, 11) as usize];
        if day.year() == today.year() {
            format!("{wd} {} {mon}", day.day())
        } else {
            format!("{wd} {} {mon} {}", day.day(), day.year())
        }
    };
    let delta = at_ms.saturating_sub(now_ms(now));
    let rel = if delta.abs() < 30_000 {
        "now".to_string()
    } else if delta > 0 {
        format!("in {}", span(delta))
    } else {
        format!("{} ago", span(delta.saturating_neg()))
    };
    format!("{label} {:02}:{:02} ({rel})", at.hour(), at.minute())
}

/// `3d`, `1d 4h`, `14h`, `2h 5m`, `45m` — rounded to the nearest minute;
/// minutes are dropped from 10 hours up, hours from 2 days up.
fn span(ms: i64) -> String {
    let mins = ms.saturating_add(MINUTE_MS / 2) / MINUTE_MS;
    let (days, hours, m) = (mins / 1440, (mins % 1440) / 60, mins % 60);
    if days >= 2 {
        format!("{days}d")
    } else if days == 1 {
        if hours > 0 {
            format!("1d {hours}h")
        } else {
            "1d".to_string()
        }
    } else if hours >= 10 || (hours > 0 && m == 0) {
        format!("{hours}h")
    } else if hours > 0 {
        format!("{hours}h {m}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn zoned(s: &str, tz: &str) -> Zoned {
        let dt: DateTime = s.parse().unwrap();
        dt.in_tz(tz).unwrap()
    }

    fn local(at_ms: i64, tz: &str) -> String {
        let z = Timestamp::from_millisecond(at_ms)
            .unwrap()
            .in_tz(tz)
            .unwrap();
        z.datetime().strftime("%Y-%m-%dT%H:%M").to_string()
    }

    fn madrid_now() -> Zoned {
        // Friday.
        zoned("2026-10-30T10:00", "Europe/Madrid")
    }

    fn at(tokens: &[&str], now: &Zoned) -> Option<String> {
        parse_when(tokens, now).map(|w| local(w.at_ms, "Europe/Madrid"))
    }

    #[test]
    fn hhmm_forms() {
        assert_eq!(parse_hhmm("9"), Some((9, 0)));
        assert_eq!(parse_hhmm("09:30"), Some((9, 30)));
        assert_eq!(parse_hhmm("9am"), Some((9, 0)));
        assert_eq!(parse_hhmm("9:15pm"), Some((21, 15)));
        assert_eq!(parse_hhmm("21:00"), Some((21, 0)));
        assert_eq!(parse_hhmm("12am"), Some((0, 0)));
        assert_eq!(parse_hhmm("12PM"), Some((12, 0)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("9:60"), None);
        assert_eq!(parse_hhmm("13pm"), None);
        assert_eq!(parse_hhmm("9:5"), None);
        assert_eq!(parse_hhmm(""), None);
        assert_eq!(parse_hhmm("noon"), None);
    }

    #[test]
    fn relative_compound_durations() {
        let now = madrid_now();
        let w = parse_when(&["in", "2h30m"], &now).unwrap();
        assert_eq!(w.at_ms - now_ms(&now), 9_000_000);
        assert_eq!(w.consumed, 2);
        let w = parse_when(&["90m", "stretch"], &now).unwrap();
        assert_eq!(w.at_ms - now_ms(&now), 5_400_000);
        assert_eq!(w.consumed, 1);
        assert!(parse_when(&["in", "367d"], &now).is_none());
        assert!(parse_when(&["in", "0s"], &now).is_none());
        assert!(parse_when(&["in", "h"], &now).is_none());
        assert!(parse_when(&["in", "2h30"], &now).is_none());
    }

    #[test]
    fn clock_today_or_tomorrow_rule() {
        let now = madrid_now();
        assert_eq!(at(&["at", "9am"], &now).unwrap(), "2026-10-31T09:00");
        assert_eq!(at(&["18:30"], &now).unwrap(), "2026-10-30T18:30");
        assert_eq!(at(&["at", "18"], &now).unwrap(), "2026-10-30T18:00");
        // A bare number without `at` is reminder text, not a clock.
        assert!(parse_when(&["5", "apples"], &now).is_none());
    }

    #[test]
    fn days_and_weekdays() {
        let now = madrid_now();
        assert_eq!(at(&["fri", "17:00"], &now).unwrap(), "2026-10-30T17:00");
        assert_eq!(
            at(&["Friday", "at", "5pm"], &now).unwrap(),
            "2026-10-30T17:00"
        );
        // Weekday equal to today but already past → next week.
        assert_eq!(at(&["fri", "9:00"], &now).unwrap(), "2026-11-06T09:00");
        assert_eq!(at(&["fri"], &now).unwrap(), "2026-11-06T09:00");
        assert_eq!(at(&["mon"], &now).unwrap(), "2026-11-02T09:00");
        assert_eq!(at(&["tomorrow"], &now).unwrap(), "2026-10-31T09:00");
        assert_eq!(at(&["tonight"], &now).unwrap(), "2026-10-30T20:00");
        assert_eq!(at(&["tonight", "9"], &now).unwrap(), "2026-10-30T21:00");
        assert_eq!(at(&["today", "18:00"], &now).unwrap(), "2026-10-30T18:00");
        // Bare `today` means 09:00, already past.
        assert!(parse_when(&["today"], &now).is_none());
        assert!(parse_when(&["today", "8am"], &now).is_none());
    }

    #[test]
    fn dates() {
        let now = madrid_now();
        assert_eq!(at(&["2026-11-03"], &now).unwrap(), "2026-11-03T09:00");
        assert_eq!(
            at(&["2026-11-03", "10:00"], &now).unwrap(),
            "2026-11-03T10:00"
        );
        assert_eq!(
            at(&["2026-11-03", "at", "10:00"], &now).unwrap(),
            "2026-11-03T10:00"
        );
        // Past, invalid and too-far dates.
        assert!(parse_when(&["2026-10-01"], &now).is_none());
        assert!(parse_when(&["2026-02-30"], &now).is_none());
        assert!(parse_when(&["2028-01-01"], &now).is_none());
        assert!(parse_when(&["20261103"], &now).is_none());
    }

    #[test]
    fn consumed_counts_and_longest_match() {
        let now = madrid_now();
        let w = parse_when(&["tomorrow", "9am", "call", "mom"], &now).unwrap();
        assert_eq!(w.consumed, 2);
        assert_eq!(local(w.at_ms, "Europe/Madrid"), "2026-10-31T09:00");
        let w = parse_when(&["tomorrow", "at", "9am", "call"], &now).unwrap();
        assert_eq!(w.consumed, 3);
        let w = parse_when(&["tomorrow", "call", "mom"], &now).unwrap();
        assert_eq!(w.consumed, 1);
        assert_eq!(local(w.at_ms, "Europe/Madrid"), "2026-10-31T09:00");
        let w = parse_when(&["in", "2h", "tea"], &now).unwrap();
        assert_eq!(w.consumed, 2);
    }

    #[test]
    fn invalid_inputs_are_none() {
        let now = madrid_now();
        assert!(parse_when(&[], &now).is_none());
        assert!(parse_when(&["call", "mom"], &now).is_none());
        assert!(parse_when(&["at"], &now).is_none());
        assert!(parse_when(&["in"], &now).is_none());
        assert!(parse_when(&["someday"], &now).is_none());
        // An invalid clock is not consumed: only the day matches.
        assert_eq!(parse_when(&["fri", "25:00"], &now).unwrap().consumed, 1);
    }

    #[test]
    fn new_york_dst_change() {
        // US DST ends 2026-11-01 02:00 → 01:00.
        let now = zoned("2026-10-31T10:00", "America/New_York");
        let w = parse_when(&["tomorrow", "9am"], &now).unwrap();
        assert_eq!(local(w.at_ms, "America/New_York"), "2026-11-01T09:00");
        // 23 wall hours + the extra repeated hour = 24 h of real time.
        assert_eq!(w.at_ms - now_ms(&now), 24 * 3_600_000);
        let next = next_local(
            9,
            0,
            &DayRule::Daily,
            &zoned("2026-11-01T00:30", "America/New_York"),
        );
        assert_eq!(local(next, "America/New_York"), "2026-11-01T09:00");
        let sun = next_local(8, 0, &DayRule::Weekdays(vec![Weekday::Sunday]), &now);
        assert_eq!(local(sun, "America/New_York"), "2026-11-01T08:00");
    }

    #[test]
    fn next_local_across_madrid_dst() {
        // Europe DST ends 2026-10-25 03:00 → 02:00.
        let now = zoned("2026-10-25T07:30", "Europe/Madrid");
        let next = next_local(8, 0, &DayRule::Daily, &now);
        assert_eq!(local(next, "Europe/Madrid"), "2026-10-25T08:00");
        assert_eq!(next - now_ms(&now), 1_800_000);
        // From the day before, the next 08:00 is 25 h away in real time.
        let before = zoned("2026-10-24T08:00", "Europe/Madrid");
        let next = next_local(8, 0, &DayRule::Daily, &before);
        assert_eq!(local(next, "Europe/Madrid"), "2026-10-25T08:00");
        assert_eq!(next - now_ms(&before), 25 * 3_600_000);
        // Weekday anchored: Friday 10:00 asking for Friday 09:00 → next week.
        let fri = next_local(
            9,
            0,
            &DayRule::Weekdays(vec![Weekday::Friday]),
            &madrid_now(),
        );
        assert_eq!(local(fri, "Europe/Madrid"), "2026-11-06T09:00");
    }

    #[test]
    fn describe_shapes() {
        let now = zoned("2026-10-18T16:25", "Europe/Madrid");
        let base = now_ms(&now);
        assert_eq!(
            describe(base + 2 * 3_600_000 + 5 * 60_000, &now),
            "today 18:30 (in 2h 5m)"
        );
        let tomorrow9 = parse_when(&["tomorrow"], &now).unwrap().at_ms;
        assert_eq!(describe(tomorrow9, &now), "tomorrow 09:00 (in 16h)");
        let mon = parse_when(&["2026-10-26"], &now).unwrap().at_ms;
        assert_eq!(describe(mon, &now), "Mon 26 Oct 09:00 (in 7d)");
        assert_eq!(describe(base - 5 * 60_000, &now), "today 16:20 (5m ago)");
        assert_eq!(describe(base + 10_000, &now), "today 16:25 (now)");
        assert_eq!(describe(base + 3 * 3_600_000, &now), "today 19:25 (in 3h)");
        let next_year = parse_when(&["2027-01-04"], &now).unwrap().at_ms;
        assert!(describe(next_year, &now).starts_with("Mon 4 Jan 2027 09:00"));
    }

    #[test]
    fn recurrence_grammar_covers_every_form() {
        let rec = |s: &str| {
            let t: Vec<&str> = s.split_whitespace().collect();
            parse_recurrence(&t).map(|(r, c, n)| (r.describe(), c, n))
        };
        assert_eq!(rec("day at 08:00 x"), Some(("daily".into(), (8, 0), 3)));
        assert_eq!(
            rec("weekday at 9 standup"),
            Some(("weekdays".into(), (9, 0), 3))
        );
        assert_eq!(rec("weekend 10am"), Some(("weekends".into(), (10, 0), 2)));
        assert_eq!(
            rec("mon,wed,fri at 18:00 gym"),
            Some(("mon,wed,fri".into(), (18, 0), 3))
        );
        assert_eq!(rec("Fri at 17:30"), Some(("fri".into(), (17, 30), 3)));
        assert_eq!(
            rec("month on the 1st at 9 rent"),
            Some(("monthly on the 1st".into(), (9, 0), 6))
        );
        assert_eq!(
            rec("month on 22 at 09:30"),
            Some(("monthly on the 22nd".into(), (9, 30), 5))
        );
        assert_eq!(rec("month on the 32nd at 9"), None);
        assert_eq!(rec("mon,funday at 9"), None);
        assert_eq!(rec("1h ping"), None, "intervals are not anchored");
    }

    #[test]
    fn rules_pick_the_right_next_day() {
        // Thu 2026-10-29 10:00 Madrid.
        let now = zoned("2026-10-29T10:00", "Europe/Madrid");
        let day_of = |ms: i64| {
            let z = jiff::Timestamp::from_millisecond(ms)
                .unwrap()
                .to_zoned(now.time_zone().clone());
            (z.month(), z.day(), z.hour())
        };
        let weekdays = DayRule::Weekdays(vec![
            Weekday::Monday,
            Weekday::Tuesday,
            Weekday::Wednesday,
            Weekday::Thursday,
            Weekday::Friday,
        ]);
        // 09:00 already passed Thursday → Friday 30th.
        assert_eq!(day_of(next_local(9, 0, &weekdays, &now)), (10, 30, 9));
        let mwf = DayRule::Weekdays(vec![Weekday::Monday, Weekday::Wednesday, Weekday::Friday]);
        assert_eq!(day_of(next_local(9, 0, &mwf, &now)), (10, 30, 9));
        // Month day 31 in November (30 days) fires on the 30th.
        let month_end = DayRule::MonthDay(31);
        assert_eq!(day_of(next_local(9, 0, &month_end, &now)), (10, 31, 9));
        let after = zoned("2026-10-31T10:00", "Europe/Madrid");
        let z = jiff::Timestamp::from_millisecond(next_local(9, 0, &month_end, &after))
            .unwrap()
            .to_zoned(after.time_zone().clone());
        assert_eq!((z.month(), z.day()), (11, 30));
        // DST: Madrid leaves summer time on Sun 2026-10-25 at 03:00; a
        // weekly Sunday 09:00 from the 18th to the 25th stays 09:00 local.
        let before = zoned("2026-10-17T12:00", "Europe/Madrid");
        let sun = DayRule::Weekdays(vec![Weekday::Sunday]);
        let first = next_local(9, 0, &sun, &before);
        let z1 = jiff::Timestamp::from_millisecond(first)
            .unwrap()
            .to_zoned(before.time_zone().clone());
        assert_eq!((z1.day(), z1.hour()), (18, 9));
        let second = next_local(9, 0, &sun, &z1);
        let z2 = jiff::Timestamp::from_millisecond(second)
            .unwrap()
            .to_zoned(before.time_zone().clone());
        assert_eq!((z2.month(), z2.day(), z2.hour()), (10, 25, 9));
        assert_eq!(
            second - first,
            7 * DAY_MS + 3_600_000,
            "a 169 h week across the change"
        );
    }
}
