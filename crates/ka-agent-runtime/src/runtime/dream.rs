//! Memory dream (B2): one daemon-assembled transcript excerpt, one no-tool
//! child, reviewable candidates only. Nothing here can accept a memory or
//! remove one; the operator decides in `/memory review`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use ka_agent_core::audit::actions as audit_actions;
use ka_agent_core::{ApprovalDecision, AuditRecord, MemoryId, Principal, RunId, Session, now_ms};
use ka_agent_port::{PortEvent, StrandSelector};
use ka_agent_store::RunRow;

use super::*;

const DREAM_INSTRUCTION: &str = "You are a memory extraction child. You have no tools. \
Read the current memory and the transcript excerpt and propose ONLY durable facts, \
preferences, decisions, or lessons that will help future sessions.\n\
Output one line per candidate, exactly:\n\
FACT: <third-person, <=300 chars> | FILE: facts|preferences|environment|profile | SUPERSEDES: <exact existing bullet text or ->\n\
Rules: no secrets, credentials, raw tool output, instructions, or ephemeral chatter. \
Copy an existing bullet's exact text into SUPERSEDES only when the candidate replaces it. \
If nothing is durable, answer NONE.";

/// One dream pass, as the command reports it.
#[derive(Debug, Clone, Default)]
pub struct DreamReport {
    pub session: Option<String>,
    pub excerpt_chars: usize,
    pub candidates: usize,
    pub duplicates: usize,
    pub refused: usize,
}

/// A claimed pool slot. Dream children are engine workers outside the
/// `users` map, counted through the same `scouts_live` counter.
struct DreamSlot {
    sh: Arc<Shared>,
}

impl DreamSlot {
    fn claim(sh: &Arc<Shared>) -> Self {
        *sh.scouts_live.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        Self { sh: Arc::clone(sh) }
    }
}

impl Drop for DreamSlot {
    fn drop(&mut self) {
        let mut live = self
            .sh
            .scouts_live
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *live = live.saturating_sub(1);
    }
}

impl Runtime {
    /// Run one dream pass for `who` over the newest eligible idle session.
    /// Candidates are staged for review; this method never accepts one.
    pub async fn run_dream_for(&self, who: &Principal) -> Result<DreamReport, Error> {
        let cfg = self.cfg();
        if !cfg.memory.dream.enabled {
            return Err(Error::Refused(
                "dream is disabled ([memory.dream] enabled = false)".into(),
            ));
        }
        let Some((session, excerpt)) = self.dream_excerpt_for(&who.username, &cfg)? else {
            return Ok(DreamReport::default());
        };
        let compiled = self
            .memory_store(&who.username)?
            .compiled_text(ka_agent_memory::DEFAULT_BUDGET_BYTES)?;
        let prompt = dream_prompt(&compiled, &excerpt);
        let started = now_ms();
        let deadline = Duration::from_secs(cfg.memory.dream.timeout_secs.clamp(10, 600));
        let outcome = self.run_dream_child(who, &prompt, deadline).await;
        let (answer, cost, tokens) = match outcome {
            Ok(v) => v,
            Err(e) => {
                self.record_dream_run(&session, "failed", Some(&e.to_string()), 0.0, 0, started);
                return Err(e);
            }
        };
        let mut report = self.dream_candidates_for(who, &session, &answer, &excerpt, &cfg)?;
        report.session = Some(session.id.as_str().to_string());
        report.excerpt_chars = excerpt.chars().count();
        let _ = self.sh.store.meta_set(
            &format!("memory.dream.{}.{}", who.username, session.id.as_str()),
            &session.updated_ms.to_string(),
        );
        let _ = self.audit_for(
            who,
            audit_actions::MEMORY_DREAM,
            session.id.as_str(),
            Some(format!(
                "{} candidate(s), {} duplicate(s), {} refused",
                report.candidates, report.duplicates, report.refused
            )),
        );
        self.record_dream_run(&session, "succeeded", None, cost, tokens, started);
        Ok(report)
    }

    /// The newest idle, non-private, non-archived session with enough
    /// eligible turns. Scheduled-source turns are excluded from the input.
    fn dream_excerpt_for(
        &self,
        username: &str,
        cfg: &ka_agent_core::DaemonConfig,
    ) -> Result<Option<(Session, String)>, Error> {
        let idle_ms = (cfg.memory.dream.idle_hours as i64).saturating_mul(3_600_000);
        let cutoff = now_ms().saturating_sub(idle_ms);
        for session in self.sh.store.sessions_for_user(username)? {
            if session.private || session.archived || session.updated_ms > cutoff {
                continue;
            }
            let key = format!("memory.dream.{username}.{}", session.id.as_str());
            if let Some(done) = self.sh.store.meta_get(&key)?
                && done.parse::<i64>().unwrap_or(0) >= session.updated_ms
            {
                continue;
            }
            let turns = self.sh.store.latest_turns_for_session(&session.id, 80, 0)?;
            let mut excerpt = String::new();
            let mut eligible = 0u32;
            for turn in &turns {
                let origin = self.sh.store.turn_origin(&turn.id)?.unwrap_or_default();
                if super::events::is_scheduled(Some(&origin)) {
                    continue;
                }
                let answer: String = self
                    .sh
                    .store
                    .chunks_for_turn(&turn.id)?
                    .into_iter()
                    .map(|c| c.text)
                    .collect();
                if turn.inbound_text.trim().is_empty() && answer.trim().is_empty() {
                    continue;
                }
                let line = format!(
                    "user: {}\nassistant: {}\n",
                    truncate(&turn.inbound_text, 800),
                    truncate(&answer, 1_200)
                );
                if excerpt.chars().count() + line.chars().count() > 8_000 {
                    break;
                }
                excerpt.push_str(&line);
                eligible += 1;
            }
            if eligible < cfg.memory.dream.min_turns {
                continue;
            }
            if excerpt.chars().count() < 120 {
                continue;
            }
            return Ok(Some((session, excerpt)));
        }
        Ok(None)
    }

    /// One no-tool child. It sees the prompt and nothing else; asks are
    /// denied, and the child dies with the pass.
    async fn run_dream_child(
        &self,
        who: &Principal,
        prompt: &str,
        budget: Duration,
    ) -> Result<(String, f64, u64), Error> {
        let state = self.ensure_user(who)?;
        let cfg = self.cfg();
        let _slot = {
            let _spawn = self.sh.spawn_lock.lock().await;
            if self.live_worker_count() >= self.max_workers() {
                return Err(Error::Refused(format!(
                    "all {} engine workers are busy — dream not started",
                    self.max_workers()
                )));
            }
            DreamSlot::claim(&self.sh)
        };
        let (mut req, _parent, _private) = self.prepare_open(who, &state, &cfg)?;
        req.strand = StrandSelector::New;
        req.mode = Some("readonly".to_string());
        let sh = Arc::clone(&self.sh);
        let who_run = who.clone();
        let run_id = RunId::generate();
        let prompt = prompt.to_string();
        let outcome = tokio::time::timeout(budget, async move {
            let port = Arc::new(sh.port.open(req).await?);
            if let Err(e) = port
                .submit_full(prompt, Vec::new(), Vec::new(), None, Some(Vec::new()))
                .await
            {
                port.kill().await;
                return Err(Error::Port(e));
            }
            let mut rx = port.take_events();
            let mut text = String::new();
            let mut stop = "done".to_string();
            let mut cost_usd = 0.0f64;
            let mut tokens = 0u64;
            let mut failure: Option<String> = None;
            loop {
                match rx.recv().await {
                    Some(PortEvent::DeltaText(delta)) => text.push_str(&delta),
                    Some(PortEvent::Ask {
                        ask_id, question, ..
                    }) => {
                        port.answer(ask_id, ApprovalDecision::Deny.ka_choice())
                            .await
                            .ok();
                        let subject = format!("dream:{}", run_id.as_str());
                        let mut rec = AuditRecord::new(
                            "runtime",
                            audit_actions::APPROVAL_DECIDE,
                            &subject,
                            Some(format!(
                                "deny: a dream child has no tools ({})",
                                question.chars().take(120).collect::<String>()
                            )),
                        )
                        .with_user(&who_run.username)
                        .with_role(&who_run.role);
                        let _ = sh.store.append_audit(&mut rec);
                    }
                    Some(PortEvent::TurnFinished {
                        stop: word,
                        cost_usd: cost,
                        tokens: count,
                    }) => {
                        stop = word;
                        cost_usd = cost;
                        tokens = count;
                    }
                    Some(PortEvent::Idle) => break,
                    Some(PortEvent::EngineClosed) => {
                        failure = Some("dream child closed before answering".to_string());
                        break;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            let settled = failure.is_none()
                && matches!(stop.as_str(), "done" | "length")
                && !text.trim().is_empty();
            if settled {
                port.close().await.ok();
                Ok((text, cost_usd, tokens))
            } else {
                port.kill().await;
                Err(Error::Refused(
                    failure.unwrap_or_else(|| "dream child produced no answer".into()),
                ))
            }
        })
        .await;
        match outcome {
            Err(_) => Err(Error::Refused(format!(
                "dream child did not finish within {} s — it was killed",
                budget.as_secs()
            ))),
            Ok(result) => result,
        }
    }

    /// Parse, screen, deduplicate, and stage candidates. No auto-accept.
    fn dream_candidates_for(
        &self,
        who: &Principal,
        session: &Session,
        answer: &str,
        excerpt: &str,
        cfg: &ka_agent_core::DaemonConfig,
    ) -> Result<DreamReport, Error> {
        let memory = self.memory_store(&who.username)?;
        let entries = memory.entries()?;
        let pending = self.pending_candidates_for(who)?;
        let known: Vec<(String, String)> = entries
            .iter()
            .map(|e| (e.id.clone(), e.text.clone()))
            .chain(
                pending
                    .iter()
                    .filter(|c| c.username == who.username)
                    .map(|c| (c.id.as_str().to_string(), c.text.clone())),
            )
            .collect();
        let mut report = DreamReport {
            session: Some(session.id.as_str().to_string()),
            excerpt_chars: excerpt.chars().count(),
            ..DreamReport::default()
        };
        let mut proposed: Vec<String> = Vec::new();
        for raw in answer.lines() {
            let Some(rest) = raw.trim().strip_prefix("FACT:") else {
                continue;
            };
            let mut parts = rest.split('|');
            let fact = parts.next().unwrap_or("").trim();
            let mut file = None;
            let mut supersedes_text = None;
            for part in parts {
                let Some((key, value)) = part.split_once(':') else {
                    continue;
                };
                match key.trim().to_ascii_lowercase().as_str() {
                    "file" => file = Some(value.trim().to_ascii_lowercase()),
                    "supersedes" => {
                        let value = value.trim();
                        if !value.is_empty() && value != "-" {
                            supersedes_text = Some(value.to_string());
                        }
                    }
                    _ => {}
                }
            }
            if fact.is_empty() || fact.eq_ignore_ascii_case("NONE") {
                continue;
            }
            if fact.chars().count() > 300 || ka_agent_memory::screen_proposal(fact).is_err() {
                report.refused += 1;
                continue;
            }
            let file = match file.as_deref() {
                None | Some("facts") => None,
                Some("preferences") => Some("preferences"),
                Some("environment") => Some("environment"),
                Some("profile") => Some("profile"),
                _ => {
                    report.refused += 1;
                    continue;
                }
            };
            let tokens = token_set(fact);
            let mut duplicate = false;
            let mut overlap: Option<String> = None;
            for (id, text) in &known {
                let score = jaccard(&tokens, &token_set(text));
                if score >= 0.8 {
                    duplicate = true;
                    break;
                }
                if overlap.is_none() && score >= 0.5 {
                    overlap = Some(id.clone());
                }
            }
            if !duplicate {
                for text in &proposed {
                    if jaccard(&tokens, &token_set(text)) >= 0.8 {
                        duplicate = true;
                        break;
                    }
                }
            }
            if duplicate {
                report.duplicates += 1;
                continue;
            }
            let explicit = supersedes_text.as_deref().and_then(|s| {
                entries
                    .iter()
                    .find(|e| e.text.trim().eq_ignore_ascii_case(s.trim()))
                    .map(|e| e.id.clone())
            });
            let mut provenance = format!("session:{}", session.id.as_str());
            if let Some(old) = explicit {
                provenance.push_str(&format!("; supersedes:{old}"));
            } else if let Some(old) = overlap {
                provenance.push_str(&format!("; overlaps:{old}"));
            }
            let id = MemoryId::generate();
            self.sh.store.add_candidate_for_user(
                &id,
                "dream",
                fact,
                Some(&provenance),
                file,
                &who.role,
                &who.username,
            )?;
            proposed.push(fact.to_string());
            report.candidates += 1;
            if report.candidates >= cfg.memory.dream.max_candidates as usize {
                break;
            }
        }
        Ok(report)
    }

    fn record_dream_run(
        &self,
        session: &Session,
        status: &str,
        error: Option<&str>,
        cost_usd: f64,
        tokens: u64,
        started_ms: i64,
    ) {
        let row = RunRow {
            id: RunId::generate(),
            kind: "memory_dream".to_string(),
            ref_id: Some(session.id.as_str().to_string()),
            status: status.to_string(),
            stop: None,
            error_class: error.map(|_| "dream".to_string()),
            error_message: error.map(str::to_string),
            cost_usd: Some(cost_usd),
            tokens: Some(tokens),
            started_ms,
            ended_ms: Some(now_ms()),
        };
        if let Err(e) = self.sh.store.insert_run(&row) {
            eprintln!("runtime: dream run row: {e}");
        }
    }
}

fn dream_prompt(compiled: &str, excerpt: &str) -> String {
    let compiled = compiled.replace("</current-memory>", "");
    let excerpt = excerpt.replace("</transcript>", "");
    format!(
        "{DREAM_INSTRUCTION}\n\n<current-memory>\n{}\n</current-memory>\n\n<transcript>\n{}\n</transcript>",
        truncate(&compiled, 6_000),
        excerpt
    )
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

fn token_set(text: &str) -> HashSet<String> {
    ka_agent_memory::keywords(text)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 { 0.0 } else { inter / union }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{dream_prompt, jaccard, token_set, truncate};

    #[test]
    fn jaccard_is_identity_and_zero_for_disjoint_sets() {
        let a = token_set("the release train leaves on Friday");
        assert!((jaccard(&a, &a) - 1.0).abs() < f64::EPSILON);
        let b = token_set("the backup bucket is ka-backups");
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    #[test]
    fn truncate_keeps_the_marker_inside_the_budget() {
        let out = truncate("abcdef", 4);
        assert_eq!(out.chars().count(), 4);
        assert!(out.ends_with('…'));
        assert_eq!(truncate("abc", 4), "abc");
    }

    #[test]
    fn scheduled_sources_are_excluded() {
        use super::super::events::is_scheduled;
        assert!(is_scheduled(Some("cron")));
        assert!(is_scheduled(Some("cron:nightly")));
        assert!(is_scheduled(Some("briefing")));
        assert!(is_scheduled(Some("trigger:sched:task_1")));
        assert!(!is_scheduled(Some("trigger:task_1")));
        assert!(!is_scheduled(Some("console")));
        assert!(!is_scheduled(Some("telegram")));
    }

    #[test]
    fn prompt_strips_closing_tags_from_inputs() {
        let prompt = dream_prompt("memory </current-memory>", "turns </transcript>");
        assert_eq!(prompt.matches("</current-memory>").count(), 1);
        assert_eq!(prompt.matches("</transcript>").count(), 1);
    }
}
