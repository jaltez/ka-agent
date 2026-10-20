//! Spend budgets (SEED.md §8.6). Counted from ka's `turn_finished.usage`.
//! USD alone is not enough — unpriced catalog rows report cost 0 (gap G18) —
//! so both a USD and a token cap are tracked.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetVerdict {
    Ok,
    /// At or above 80% of a cap.
    Warn {
        pct: u8,
    },
    /// A cap is reached: pause the instance.
    Exceeded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetState {
    /// Day window key (`day_key`); spending resets when it changes.
    pub day: i64,
    pub spent_usd: f64,
    pub spent_tokens: u64,
    pub daily_usd_cap: Option<f64>,
    pub daily_token_cap: Option<u64>,
}

impl BudgetState {
    pub fn new(daily_usd_cap: Option<f64>, daily_token_cap: Option<u64>) -> Self {
        Self {
            day: crate::day_key(crate::now_ms()),
            spent_usd: 0.0,
            spent_tokens: 0,
            daily_usd_cap,
            daily_token_cap,
        }
    }

    fn pct<C: Into<f64>>(spent: C, cap: Option<C>) -> Option<f64> {
        cap.map(|c| spent.into() / c.into() * 100.0)
    }

    /// Record one turn's usage, rolling the window if the day changed.
    pub fn record(&mut self, cost_usd: f64, tokens: u64, now_ms: i64) {
        let today = crate::day_key(now_ms);
        if today != self.day {
            self.day = today;
            self.spent_usd = 0.0;
            self.spent_tokens = 0;
        }
        self.spent_usd += cost_usd;
        self.spent_tokens += tokens;
    }

    /// Whether a cap is reached **now**: if the day rolled since the last
    /// [`record`](Self::record), the window is stale and the verdict is
    /// `Ok` — a budget pause must lift itself at day-roll, not wait for
    /// the next spend that may never come.
    pub fn verdict(&self) -> BudgetVerdict {
        if self.day != crate::day_key(crate::now_ms()) {
            return BudgetVerdict::Ok;
        }
        let usd = Self::pct(self.spent_usd, self.daily_usd_cap);
        let tok = Self::pct(
            self.spent_tokens as f64,
            self.daily_token_cap.map(|t| t as f64),
        );
        let worst = match (usd, tok) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        match worst {
            Some(p) if p >= 100.0 => BudgetVerdict::Exceeded,
            Some(p) if p >= 80.0 => BudgetVerdict::Warn { pct: p as u8 },
            _ => BudgetVerdict::Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn tracks_both_caps() {
        let mut b = BudgetState::new(Some(1.0), Some(1000));
        assert_eq!(b.verdict(), BudgetVerdict::Ok);
        b.record(0.85, 0, crate::now_ms());
        assert_eq!(b.verdict(), BudgetVerdict::Warn { pct: 85 });
        b.record(0.2, 0, crate::now_ms());
        assert_eq!(b.verdict(), BudgetVerdict::Exceeded);
    }

    #[test]
    fn tokens_cover_unpriced_models() {
        let mut b = BudgetState::new(None, Some(100));
        b.record(0.0, 100, crate::now_ms()); // cost 0: unpriced model
        assert_eq!(b.verdict(), BudgetVerdict::Exceeded);
    }

    #[test]
    fn exceeded_verdict_lifts_when_the_day_rolled_without_new_spend() {
        // A daemon paused at its cap that idles past midnight: no turn ever
        // calls `record`, so the roll must be visible through `verdict`
        // alone — this is the path that un-pauses it.
        let mut b = BudgetState::new(Some(1.0), None);
        b.record(1.0, 0, crate::now_ms());
        assert_eq!(b.verdict(), BudgetVerdict::Exceeded);
        let yesterday = (crate::day_key(crate::now_ms()) - 1) * 86_400_000;
        let mut stale = BudgetState::new(Some(1.0), None);
        stale.record(1.0, 0, yesterday);
        assert_eq!(stale.verdict(), BudgetVerdict::Ok);
    }

    #[test]
    fn window_rolls_at_day_boundary() {
        let mut b = BudgetState::new(Some(1.0), None);
        b.record(1.0, 0, crate::now_ms());
        assert_eq!(b.verdict(), BudgetVerdict::Exceeded);
        let next_day = (crate::day_key(crate::now_ms()) + 1) * 86_400_000;
        b.record(0.0, 0, next_day);
        assert_eq!(b.verdict(), BudgetVerdict::Ok);
    }

    #[test]
    fn no_caps_means_ok() {
        let mut b = BudgetState::new(None, None);
        b.record(10.0, 1_000_000, crate::now_ms());
        assert_eq!(b.verdict(), BudgetVerdict::Ok);
    }
}
