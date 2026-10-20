//! `ka-agent usage`: what the agent spent, per day, from the store's
//! recorded turns — the daemon need not be running.

use std::process::ExitCode;

use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

pub fn usage(days: u32) -> ExitCode {
    let paths = Paths::new(Paths::default_root());
    if !paths.state_db().exists() {
        eprintln!(
            "ka-agent: no {} yet — nothing recorded",
            paths.state_db().display()
        );
        return ExitCode::FAILURE;
    }
    let store = match Store::open(&paths.state_db()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ka-agent: {}: {e}", paths.state_db().display());
            return ExitCode::FAILURE;
        }
    };
    let cfg = ka_agent_core::DaemonConfig::load(&crate::default_config_path()).ok();
    let tz = cfg
        .as_ref()
        .map(|c| c.time_zone())
        .unwrap_or_else(jiff::tz::TimeZone::system);
    let days = days.clamp(1, 366) as i64;
    let now = jiff::Timestamp::now();
    let since = now
        .checked_sub(jiff::Span::new().days(days))
        .map_or(0, |t| t.as_millisecond());

    let rows = match store.usage_rows_since(since) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ka-agent: store: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Bucket by local calendar day; a turn lands on the day it started.
    let mut days_usage: Vec<(String, u64, u64, f64)> = Vec::new();
    let (mut turns, mut tokens, mut cost) = (0u64, 0u64, 0.0);
    for (created_ms, c, t) in rows {
        let day = jiff::Timestamp::from_millisecond(created_ms)
            .map(|ts| ts.to_zoned(tz.clone()))
            .map(|z| z.strftime("%Y-%m-%d").to_string())
            .unwrap_or_else(|_| "?".to_string());
        match days_usage.last_mut() {
            Some(last) if last.0 == day => {
                last.1 += 1;
                last.2 += t;
                last.3 += c;
            }
            _ => days_usage.push((day, 1, t, c)),
        }
        turns += 1;
        tokens += t;
        cost += c;
    }
    let rule = "-".repeat(34);
    println!("{rule}");
    println!("{:<12}{:>7}{:>12}{:>10}", "day", "turns", "tokens", "cost");
    for (day, t, tok, c) in &days_usage {
        println!("{day:<12}{t:>7}{tok:>12}{c:>10.4}");
    }
    println!("{rule}");
    println!("{:<12}{turns:>7}{tokens:>12}{cost:>10.4}", "total");
    if let Some(budget) = cfg
        .as_ref()
        .and_then(|c| c.budgets.daily_usd)
        .filter(|b| *b > 0.0)
    {
        let today = days_usage.last().map_or(0.0, |(_, _, _, c)| *c);
        println!(
            "\ntoday ${today:.4} of ${budget:.2} daily budget{}",
            if today > budget { "  — over" } else { "" }
        );
    }
    ExitCode::SUCCESS
}
