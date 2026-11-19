//! Durable allow-rule persistence (v4 B7): insert, list, remove. The
//! runtime loads a subject's rules fresh at every ask — never cached.

use super::*;
use ka_agent_core::rules::{Rule, RuleShape};

impl Store {
    /// Persist a validated rule. The caller owns validation (shape
    /// `validate()` + `passes_cases()`); the store is the dumb half.
    pub fn insert_rule(&self, r: &Rule) -> Result<()> {
        let (kind, pattern, detail) = match &r.shape {
            RuleShape::AskKey { question, detail } => ("ask_key", question.clone(), detail.clone()),
            RuleShape::CallKey { command } => ("call_key", command.clone(), None),
            RuleShape::Tool { name } => ("tool", name.clone(), None),
            RuleShape::Command { prefix } => ("command", prefix.clone(), None),
        };
        let conn = self.lock();
        conn.execute(
            "INSERT INTO grant_rules(id, subject, kind, pattern, detail, mode, from_approval, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                r.id,
                r.subject,
                kind,
                pattern,
                detail,
                r.mode,
                r.from_approval,
                r.created_ms
            ],
        )?;
        Ok(())
    }

    /// One subject's rules, oldest first (deterministic `/rules` and
    /// first-match-wins consultation).
    pub fn rules_for_subject(&self, subject: &str) -> Result<Vec<Rule>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, subject, kind, pattern, detail, mode, from_approval, created_ms
             FROM grant_rules WHERE subject = ?1 ORDER BY created_ms, id",
        )?;
        let rows = stmt.query_map(params![subject], |r| {
            let id: String = r.get(0)?;
            let kind: String = r.get(2)?;
            let pattern: String = r.get(3)?;
            let detail: Option<String> = r.get(4)?;
            let mode: String = r.get(5)?;
            let from_approval: Option<String> = r.get(6)?;
            let created_ms: i64 = r.get(7)?;
            let shape = match kind.as_str() {
                "ask_key" => RuleShape::AskKey {
                    question: pattern,
                    detail,
                },
                "tool" => RuleShape::Tool { name: pattern },
                "call_key" => RuleShape::CallKey { command: pattern },
                _ => RuleShape::Command { prefix: pattern },
            };
            Ok(Rule {
                id,
                subject: subject.to_string(),
                shape,
                mode,
                from_approval,
                created_ms,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Remove one rule; `Ok(false)` when no such rule belonged to
    /// `subject` (a user never removes another user's rule).
    pub fn remove_rule(&self, subject: &str, id: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "DELETE FROM grant_rules WHERE id = ?1 AND subject = ?2",
            params![id, subject],
        )?;
        Ok(n > 0)
    }

    /// Every rule, oldest first — the admin's `/rules` view.
    pub fn all_rules(&self) -> Result<Vec<Rule>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, subject, kind, pattern, detail, mode, from_approval, created_ms
             FROM grant_rules ORDER BY created_ms, id",
        )?;
        let rows = stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            let subject: String = r.get(1)?;
            let kind: String = r.get(2)?;
            let pattern: String = r.get(3)?;
            let detail: Option<String> = r.get(4)?;
            let mode: String = r.get(5)?;
            let from_approval: Option<String> = r.get(6)?;
            let created_ms: i64 = r.get(7)?;
            let shape = match kind.as_str() {
                "ask_key" => RuleShape::AskKey {
                    question: pattern,
                    detail,
                },
                "tool" => RuleShape::Tool { name: pattern },
                "call_key" => RuleShape::CallKey { command: pattern },
                _ => RuleShape::Command { prefix: pattern },
            };
            Ok(Rule {
                id,
                subject,
                shape,
                mode,
                from_approval,
                created_ms,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Remove one rule by id regardless of subject (the admin's `/rules
    /// rm`). `Ok(false)` when no such rule exists.
    pub fn remove_rule_any(&self, id: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute("DELETE FROM grant_rules WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Total rule count (doctor's grants line).
    pub fn count_rules(&self) -> Result<i64> {
        let conn = self.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM grant_rules", [], |r| r.get(0))?;
        Ok(n)
    }
}
