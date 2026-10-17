//! Time helpers shared by every crate (no I/O).

/// Upper bound for any parsed duration (366 days). Keeps `now + ms` and
/// recurrence arithmetic inside i64 by construction; larger requests are
/// rejected, not wrapped.
pub const MAX_DURATION_MS: u64 = 366 * 86_400_000;

/// Parse `Ns`, `Nm`, `Nh`, `Nd` into milliseconds (max 366 days). One
/// grammar for `/remind`, `/cron` and the `cron_manage` tool.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.len() < 2 {
        return None;
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u64 = num.parse().ok()?;
    let mult = match unit {
        "s" => 1_000u64,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    let ms = n.checked_mul(mult)?;
    (ms <= MAX_DURATION_MS).then_some(ms)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn durations_parse_with_units() {
        assert_eq!(parse_duration("3s"), Some(3_000));
        assert_eq!(parse_duration("2m"), Some(120_000));
        assert_eq!(parse_duration("1h"), Some(3_600_000));
        assert_eq!(parse_duration("2d"), Some(172_800_000));
    }

    #[test]
    fn bad_durations_are_rejected() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("5"), None);
        assert_eq!(parse_duration("s"), None);
        assert_eq!(parse_duration("5x"), None);
        assert_eq!(parse_duration("-3s"), None);
        assert_eq!(parse_duration("1e9d"), None);
        // u64 overflow saturates to None instead of wrapping.
        assert_eq!(parse_duration("999999999999999999d"), None);
        // Anything beyond 366 days is refused, not wrapped.
        assert_eq!(parse_duration("367d"), None);
        assert_eq!(parse_duration("9223372036854775s"), None);
        assert_eq!(parse_duration("366d"), Some(MAX_DURATION_MS));
    }
}
