//! `/help` plus the shared time-formatting helpers used across command
//! modules and surfaces.

use super::{CmdFuture, Ctx, Outcome, help_for, help_text};

pub(super) fn run_help<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let topic = ctx.args.trim_start_matches('/');
        if topic.is_empty() {
            Outcome::Reply(help_text())
        } else {
            match help_for(topic) {
                Some(h) => Outcome::Reply(h),
                None => Outcome::Reply(format!("unknown command /{topic} — /help lists them")),
            }
        }
    })
}

/// `2026-10-30 18:30` in `now`'s zone.
pub fn local_compact(ms: i64, now: &jiff::Zoned) -> String {
    let z = jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(now.time_zone().clone()))
        .unwrap_or_else(|_| now.clone());
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        z.year(),
        z.month() as u8,
        z.day(),
        z.hour(),
        z.minute()
    )
}

/// Render an interval compactly (`1h`, `30m`, `2d`, `45s`) — re-exported
/// from core so surfaces and the runtime share one formatter.
pub fn fmt_interval(ms: i64) -> String {
    ka_agent_core::fmt_interval(ms)
}
