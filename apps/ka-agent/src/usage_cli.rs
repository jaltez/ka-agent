//! `ka-agent usage`: what the agent spent, per day, from the store's
//! recorded turns — the daemon need not be running.

use std::process::ExitCode;

use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

pub fn usage(days: u32, json: bool) -> ExitCode {
    let paths = Paths::new(Paths::default_root());
    let fail = |msg: String| {
        if json {
            println!("{}", serde_json::json!({"ok": false, "error": msg}));
        } else {
            eprintln!("ka-agent: {msg}");
        }
        ExitCode::FAILURE
    };
    if !paths.state_db().exists() {
        return fail(format!(
            "no {} yet — nothing recorded",
            paths.state_db().display()
        ));
    }
    let store = match Store::open(&paths.state_db()) {
        Ok(s) => s,
        Err(e) => return fail(format!("{}: {e}", paths.state_db().display())),
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
        Err(e) => return fail(format!("store: {e}")),
    };
    // Bucket by local calendar day and model; a turn lands on the day it
    // started. Rows without a model (written before the column existed)
    // group under `(not recorded)`.
    let mut days_usage: Vec<(String, String, u64, u64, f64)> = Vec::new();
    let (mut turns, mut tokens, mut cost) = (0u64, 0u64, 0.0);
    for (created_ms, c, t, model) in rows {
        let day = jiff::Timestamp::from_millisecond(created_ms)
            .map(|ts| ts.to_zoned(tz.clone()))
            .map(|z| z.strftime("%Y-%m-%d").to_string())
            .unwrap_or_else(|_| "?".to_string());
        let model = model.unwrap_or_else(|| "(not recorded)".to_string());
        match days_usage.last_mut() {
            Some(last) if last.0 == day && last.1 == model => {
                last.2 += 1;
                last.3 += t;
                last.4 += c;
            }
            _ => days_usage.push((day, model, 1, t, c)),
        }
        turns += 1;
        tokens += t;
        cost += c;
    }
    if json {
        let rows: Vec<serde_json::Value> = days_usage
            .iter()
            .map(|(day, model, t, tok, c)| {
                serde_json::json!({"day": day, "model": model, "turns": t, "tokens": tok, "cost_usd": c})
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "days": rows,
                "total": {"turns": turns, "tokens": tokens, "cost_usd": cost},
                "daily_budget_usd": cfg.as_ref().and_then(|c| c.budgets.daily_usd),
            })
        );
        return ExitCode::SUCCESS;
    }
    let rule = "-".repeat(62);
    println!("{rule}");
    println!(
        "{:<12}{:<26}{:>7}{:>9}{:>8}",
        "day", "model", "turns", "tokens", "cost"
    );
    for (day, model, t, tok, c) in &days_usage {
        let model: String = model.chars().take(25).collect();
        println!("{day:<12}{model:<26}{t:>7}{tok:>9}{c:>8.4}");
    }
    println!("{rule}");
    println!("{:<12}{turns:>33}{tokens:>9}{cost:>8.4}", "total");
    if let Some(budget) = cfg
        .as_ref()
        .and_then(|c| c.budgets.daily_usd)
        .filter(|b| *b > 0.0)
    {
        let today = days_usage.last().map_or(0.0, |(day, ..)| {
            days_usage
                .iter()
                .filter(|(d, ..)| d == day)
                .map(|b| b.4)
                .sum::<f64>()
        });
        println!(
            "\ntoday ${today:.4} of ${budget:.2} daily budget{}",
            if today > budget { "  — over" } else { "" }
        );
    }
    ExitCode::SUCCESS
}
