//! The runtime core: single-active-session orchestration over the port.
//!
//! Responsibilities (SEED.md §3.5–§3.8): single-flight prompt submission with
//! a durable queue, approval lifecycle with deny-on-expiry, budget
//! enforcement, lazy worker (re)start with backoff and a crash-loop breaker,
//! and delivery of exactly one final answer per prompt cycle at `idle`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ka_agent_core::audit::actions as audit_actions;
use ka_agent_core::{
    Approval, ApprovalDecision, ApprovalId, ApprovalKind, AuditRecord, BudgetState, BudgetVerdict,
    ConfigError, DaemonConfig, OutboxChunk, QueueItem, Session, SessionId, SessionStatus, Turn,
    TurnId, TurnStatus,
};
use ka_agent_port::{
    KaPort, OpenRequest, PortError, PortEvent, PortSession, StrandSelector, WorkerEnv, WorkerPort,
};
use ka_agent_store::{Store, StoreError};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::paths::Paths;
use crate::surface::{ApprovalCard, Surface};

/// Final-answer chunk ceiling before splitting (Telegram 4096 minus margin;
/// SEED.md §5.4).
pub const FINAL_CHUNK_MAX: usize = 3800;

#[derive(Debug, Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(#[from] ConfigError),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("port: {0}")]
    Port(#[from] PortError),
    #[error("memory: {0}")]
    Memory(#[from] ka_agent_memory::Error),
    #[error("illegal state transition {from} -> {to}")]
    Illegal { from: String, to: String },
    #[error("no pending approval {0}")]
    UnknownApproval(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

fn illegal_session(from: SessionStatus, to: SessionStatus) -> Error {
    Error::Illegal {
        from: format!("{from:?}"),
        to: format!("{to:?}"),
    }
}

fn illegal_turn(from: TurnStatus, to: TurnStatus) -> Error {
    Error::Illegal {
        from: format!("{from:?}"),
        to: format!("{to:?}"),
    }
}

/// Exponential backoff for worker restarts (unit-testable).
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    start_ms: u64,
    max_ms: u64,
}

impl Backoff {
    pub fn new(start_ms: u64, max_ms: u64) -> Self {
        let start = start_ms.max(1);
        Self {
            start_ms: start,
            max_ms: max_ms.max(start),
        }
    }

    pub fn delay(&self, attempt: u32) -> Duration {
        let a = attempt.min(16);
        let ms = self.start_ms.saturating_mul(1u64 << a).min(self.max_ms);
        Duration::from_millis(ms)
    }
}

/// Split a final answer into channel chunks at paragraph boundaries,
/// hard-splitting pathological single paragraphs.
pub fn split_final_text(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    if text.chars().count() <= max {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for para in text.split("\n\n") {
        // Paragraphs are never merged across a cap boundary.
        if !cur.is_empty() && cur.chars().count() + 2 + para.chars().count() > max {
            out.push(std::mem::take(&mut cur));
        }
        if para.chars().count() > max {
            // Pathological single paragraph: hard-split, keep the tail.
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let mut rest: String = para.to_string();
            while rest.chars().count() > max {
                let head: String = rest.chars().take(max).collect();
                rest = rest.chars().skip(max).collect();
                out.push(head);
            }
            cur.push_str(&rest);
        } else {
            if !cur.is_empty() {
                cur.push_str("\n\n");
            }
            cur.push_str(para);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

struct Live {
    port: Arc<PortSession>,
    /// ask_id of the engine ask behind each pending approval.
    pending_asks: HashMap<ApprovalId, String>,
    live_text: String,
    current_turn: Option<TurnId>,
}

struct Inner {
    session: Session,
    live: Option<Live>,
    budget: BudgetState,
    /// Timestamps (ms) of recent worker crashes, for the crash-loop breaker.
    crashes: Vec<i64>,
    restart_attempt: u32,
}

struct Shared {
    cfg: DaemonConfig,
    paths: Paths,
    store: Arc<Store>,
    memory: ka_agent_memory::MemoryStore,
    port: WorkerPort,
    surface: Arc<dyn Surface>,
    inner: Mutex<Inner>,
}

/// The runtime handle. Cheap to clone; the event loop and the expiry timer
/// share the same state.
#[derive(Clone)]
pub struct Runtime {
    sh: Arc<Shared>,
}

impl Runtime {
    /// Open state, reconcile a restart (SEED.md §7.1), restore the budget
    /// window from the meta table, adopt the most recent session.
    pub fn new(
        cfg: DaemonConfig,
        paths: Paths,
        store: Arc<Store>,
        port: WorkerPort,
        surface: Arc<dyn Surface>,
    ) -> Result<Self, Error> {
        paths.ensure_dirs()?;
        let interrupted = store.reconcile_running_turns(ka_agent_core::now_ms())?;
        if !interrupted.is_empty() {
            surface.on_note(&format!(
                "recovered after restart: {} interrupted turn(s) marked; nothing was re-submitted",
                interrupted.len()
            ));
        }
        let mut session = store
            .list_recent_sessions(1)?
            .into_iter()
            .next()
            .unwrap_or_else(|| Session::new(cfg.workspace_path()));
        // A fresh (or dangling) adopted session must exist as a row before
        // turns reference it.
        if store.get_session(&session.id).is_err() {
            store.insert_session(&session)?;
            session.status = ka_agent_core::SessionStatus::Idle;
        }
        let session = session;
        let mut budget = BudgetState::new(cfg.budgets.daily_usd, cfg.budgets.daily_tokens);
        if let Some(saved) = store.meta_get("budget")? {
            if let Ok(b) = serde_json::from_str::<BudgetState>(&saved) {
                if b.day == ka_agent_core::day_key(ka_agent_core::now_ms()) {
                    budget = b;
                }
            }
        }
        let memory = ka_agent_memory::MemoryStore::open(&paths.root().join("memory"))?;
        // Compile stable memory into the engine's user-tier MEMORY.md; ka
        // re-reads it per prompt (validation §5), so no worker restart is
        // needed after updates.
        memory.install_compiled(
            &paths.engine_home().join(".config/ka"),
            ka_agent_memory::DEFAULT_BUDGET_BYTES,
        )?;
        Ok(Self {
            sh: Arc::new(Shared {
                cfg,
                paths,
                store,
                memory,
                port,
                surface,
                inner: Mutex::new(Inner {
                    session,
                    live: None,
                    budget,
                    crashes: Vec::new(),
                    restart_attempt: 0,
                }),
            }),
        })
    }

    // ---- introspection -------------------------------------------------

    pub fn session_id(&self) -> SessionId {
        self.lock().session.id.clone()
    }

    pub fn cfg(&self) -> &DaemonConfig {
        &self.sh.cfg
    }

    /// (session status, strand id, spent USD, spent tokens, recent crashes)
    pub fn status(&self) -> (SessionStatus, Option<String>, f64, u64, usize) {
        let g = self.lock();
        (
            g.session.status,
            g.session.strand_id.clone(),
            g.budget.spent_usd,
            g.budget.spent_tokens,
            g.crashes.len(),
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.sh.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- operator operations -------------------------------------------

    /// `/new`: drop the live worker and start a fresh session/strand.
    pub async fn new_session(&self) -> Result<SessionId, Error> {
        let live_port = {
            let mut g = self.lock();
            g.live.take().map(|l| l.port)
        };
        if let Some(port) = live_port {
            port.kill().await;
        }
        let session = {
            let mut g = self.lock();
            g.session = Session::new(self.sh.cfg.workspace_path());
            g.restart_attempt = 0;
            g.session.clone()
        };
        self.sh.store.insert_session(&session)?;
        self.audit(audit_actions::SESSION_NEW, session.id.as_str(), None)?;
        self.sh.surface.on_session_status(SessionStatus::Idle);
        Ok(session.id)
    }

    /// Inbound message: submit now when idle, otherwise enqueue durably
    /// (gap G4 — ka drops mid-cycle prompts, HTTP 200 notwithstanding).
    pub async fn handle_message(&self, text: &str, source: &str) -> Result<(), Error> {
        let submit_now = {
            let mut g = self.lock();
            match g.session.status {
                SessionStatus::Idle => {
                    let from = g.session.status;
                    g.session
                        .transition(SessionStatus::Running)
                        .map_err(|t| illegal_session(from, t))?;
                    true
                }
                SessionStatus::Running
                | SessionStatus::AwaitingApproval
                | SessionStatus::Stopping => false,
                SessionStatus::Paused => {
                    self.sh
                        .surface
                        .on_note("paused (budget or auth): message dropped; check /status");
                    false
                }
                SessionStatus::Degraded => {
                    self.sh
                        .surface
                        .on_note("worker crash loop: use /new to start a fresh session");
                    false
                }
            }
        };
        if !submit_now {
            match self.status().0 {
                SessionStatus::Running
                | SessionStatus::AwaitingApproval
                | SessionStatus::Stopping => {
                    let item = QueueItem::new(self.session_id(), text.to_string(), source);
                    self.sh.store.enqueue(&item)?;
                    let n = self.sh.store.queue_len(&self.session_id())?;
                    self.sh.surface.on_note(&format!("queued (#{n})"));
                }
                _ => {}
            }
            return Ok(());
        }
        match self.submit_text(text).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // Never strand the session in `running`: the queue would
                // swallow every later message (single-flight).
                let mut g = self.lock();
                if g.session.status == SessionStatus::Running {
                    g.session.transition(SessionStatus::Idle).ok();
                }
                drop(g);
                self.persist_session_status(SessionStatus::Idle);
                Err(e)
            }
        }
    }

    fn persist_session_status(&self, status: SessionStatus) {
        let id = self.session_id();
        if let Err(e) = self.sh.store.update_session_status(&id, status) {
            self.sh
                .surface
                .on_error("internal", &format!("session status persist: {e}"));
        }
    }

    // ---- memory & history (SEED.md §4) --------------------------------

    /// `/remember <fact>`: explicit operator acceptance (SEED.md §4.4 path
    /// 1) — append, recompile the engine's MEMORY.md, git-commit.
    pub fn remember(&self, text: &str, source: &str) -> Result<String, Error> {
        let id = self
            .sh
            .memory
            .add(ka_agent_memory::MemoryFile::Facts, text, source, None)?;
        self.refresh_compiled_memory()?;
        self.sh.memory.commit(&format!("memory: accept {id}"))?;
        self.audit(
            audit_actions::MEMORY_ACCEPT,
            &id,
            Some(text.chars().take(80).collect()),
        )?;
        Ok(id)
    }

    /// `/forget <id>`: remove the entry and recompile.
    pub fn forget(&self, id: &str) -> Result<bool, Error> {
        let removed = self.sh.memory.remove(id)?;
        if removed {
            self.refresh_compiled_memory()?;
            self.sh.memory.commit(&format!("memory: forget {id}"))?;
            self.audit(audit_actions::MEMORY_EDIT, id, Some("forget".into()))?;
        }
        Ok(removed)
    }

    /// `/memory`: what is injected and at what budget.
    pub fn memory_report(&self) -> Result<ka_agent_memory::CompileReport, Error> {
        Ok(self
            .sh
            .memory
            .compile(ka_agent_memory::DEFAULT_BUDGET_BYTES)?)
    }

    /// Accepted memory entries, newest first, for the web memory view.
    pub fn memory_entries(&self) -> Result<Vec<ka_agent_memory::Entry>, Error> {
        let mut entries = self.sh.memory.entries()?;
        entries.sort_by_key(|e| std::cmp::Reverse(e.accepted_ms));
        Ok(entries)
    }

    /// The exact text injected into the engine at the current budget.
    pub fn memory_compiled_text(&self) -> Result<String, Error> {
        Ok(self
            .sh
            .memory
            .compiled_text(ka_agent_memory::DEFAULT_BUDGET_BYTES)?)
    }

    /// Register a memory candidate for review (SEED.md §4.4 path 2; the
    /// model-side producer arrives with the connector bridge).
    pub fn propose_memory(
        &self,
        source: &str,
        text: &str,
        provenance: Option<&str>,
    ) -> Result<ka_agent_core::MemoryId, Error> {
        let id = ka_agent_core::MemoryId::generate();
        self.sh.store.add_candidate(&id, source, text, provenance)?;
        Ok(id)
    }

    /// `/memory review`: pending candidates.
    pub fn pending_candidates(&self) -> Result<Vec<ka_agent_store::CandidateRow>, Error> {
        Ok(self.sh.store.pending_candidates_full()?)
    }

    /// `/memory accept <id>`: candidate → stable memory.
    pub fn accept_candidate(&self, id: &str) -> Result<bool, Error> {
        let Some(row) = self
            .sh
            .store
            .pending_candidates_full()?
            .into_iter()
            .find(|c| c.id.as_str() == id)
        else {
            return Ok(false);
        };
        let mem_id = self.sh.memory.add(
            ka_agent_memory::MemoryFile::Facts,
            &row.text,
            &row.source,
            None,
        )?;
        self.refresh_compiled_memory()?;
        self.sh
            .memory
            .commit(&format!("memory: accept {mem_id} (candidate)"))?;
        self.sh.store.set_candidate_status(&row.id, "accepted")?;
        self.audit(
            audit_actions::MEMORY_ACCEPT,
            &mem_id,
            Some(row.text.chars().take(80).collect()),
        )?;
        Ok(true)
    }

    /// `/memory reject <id>`.
    pub fn reject_candidate(&self, id: &str) -> Result<bool, Error> {
        let pending = self.sh.store.pending_candidates()?;
        if !pending.iter().any(|c| c.as_str() == id) {
            return Ok(false);
        }
        self.sh
            .store
            .set_candidate_status(&ka_agent_core::MemoryId(id.to_string()), "rejected")?;
        self.audit(
            audit_actions::MEMORY_EDIT,
            id,
            Some("candidate rejected".into()),
        )?;
        Ok(true)
    }

    /// `/history <query>`: FTS over episodes (SEED.md §4.5).
    pub fn history_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ka_agent_store::EpisodeHit>, Error> {
        Ok(self.sh.store.search_episodes(query, limit)?)
    }

    /// `/history show <turn>`: the stored exchange.
    pub fn history_show(&self, turn_id: &str) -> Result<Option<(String, String)>, Error> {
        let Some(turn) = self
            .sh
            .store
            .get_turn(&ka_agent_core::TurnId(turn_id.to_string()))?
        else {
            return Ok(None);
        };
        let answer = self
            .sh
            .store
            .chunks_for_turn(&turn.id)?
            .into_iter()
            .map(|c| c.text)
            .collect::<Vec<_>>()
            .join("");
        Ok(Some((turn.inbound_text, answer)))
    }

    fn refresh_compiled_memory(&self) -> Result<(), Error> {
        self.sh.memory.install_compiled(
            &self.sh.paths.engine_home().join(".config/ka"),
            ka_agent_memory::DEFAULT_BUDGET_BYTES,
        )?;
        Ok(())
    }

    /// `/stop`: abort; escalate to SIGKILL after the configured grace
    /// (SEED.md §3.8).
    pub async fn stop_turn(&self) -> Result<(), Error> {
        let port = {
            let mut g = self.lock();
            let status = g.session.status;
            let live_port = g.live.as_ref().map(|l| Arc::clone(&l.port));
            match (status, live_port) {
                (SessionStatus::Running | SessionStatus::AwaitingApproval, Some(p)) => {
                    g.session
                        .transition(SessionStatus::Stopping)
                        .map_err(|t| illegal_session(status, t))?;
                    Some(p)
                }
                _ => None,
            }
        };
        let Some(port) = port else {
            self.sh.surface.on_note("nothing to stop");
            return Ok(());
        };
        self.sh.surface.on_note("stopping…");
        port.abort().await.ok();
        let grace = Duration::from_secs(self.sh.cfg.turn_grace_secs);
        let watcher = Arc::clone(&port);
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            if !watcher.is_closed() {
                watcher.kill().await;
            }
        });
        Ok(())
    }

    /// Decide a pending approval: answer the engine ask and audit (SEED
    /// §5.5: single-use; "always" is never offered from channels).
    pub async fn decide(&self, approval_id: &str, decision: ApprovalDecision) -> Result<(), Error> {
        let id = ApprovalId(approval_id.to_string());
        let (ask_id, port) = {
            let g = self.lock();
            let Some(live) = g.live.as_ref() else {
                return Err(Error::UnknownApproval(approval_id.to_string()));
            };
            let Some(ask_id) = live.pending_asks.get(&id).cloned() else {
                return Err(Error::UnknownApproval(approval_id.to_string()));
            };
            (ask_id, Arc::clone(&live.port))
        };
        let status = match decision {
            ApprovalDecision::Allow | ApprovalDecision::Always => {
                ka_agent_core::ApprovalStatus::Approved
            }
            ApprovalDecision::Deny => ka_agent_core::ApprovalStatus::Denied,
        };
        port.answer(ask_id.clone(), decision.ka_choice()).await?;
        self.sh
            .store
            .decide_approval(&id, status, ka_agent_core::now_ms())?;
        self.audit(
            audit_actions::APPROVAL_DECIDE,
            approval_id,
            Some(format!("{decision:?}")),
        )?;
        self.sh.surface.on_approval_settled(approval_id, status);
        let mut g = self.lock();
        if let Some(live) = g.live.as_mut() {
            live.pending_asks.remove(&id);
        }
        if g.session.status == SessionStatus::AwaitingApproval {
            let from = g.session.status;
            g.session
                .transition(SessionStatus::Running)
                .map_err(|t| illegal_session(from, t))?;
        }
        Ok(())
    }

    /// Expiry sweep (timer-driven): pending approvals past their TTL are
    /// denied, answered to the engine, and audited (fail closed).
    pub async fn expire_approvals(&self) -> Result<Vec<ApprovalId>, Error> {
        let expired = self.sh.store.expire_pending(ka_agent_core::now_ms())?;
        let answers: Vec<String> = {
            let g = self.lock();
            g.live
                .as_ref()
                .map(|l| {
                    expired
                        .iter()
                        .filter_map(|id| l.pending_asks.get(id).cloned())
                        .collect()
                })
                .unwrap_or_default()
        };
        for id in &expired {
            self.audit(audit_actions::APPROVAL_EXPIRE, id.as_str(), None)?;
            self.sh
                .surface
                .on_approval_settled(id.as_str(), ka_agent_core::ApprovalStatus::Expired);
        }
        if let Some(port) = self.live_port() {
            for ask_id in answers {
                port.answer(ask_id, ApprovalDecision::Deny.ka_choice())
                    .await
                    .ok();
            }
        }
        Ok(expired)
    }

    /// Periodic approval-expiry loop; run it next to the event loop.
    pub async fn run_expiry_timer(&self, every: Duration) {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // first fires immediately
        loop {
            tick.tick().await;
            if let Err(e) = self.expire_approvals().await {
                self.sh
                    .surface
                    .on_error("internal", &format!("expiry sweep: {e}"));
            }
        }
    }

    /// Decide the most recent pending approval (console `/y` and `/n`).
    /// Returns Ok(false) when nothing is pending.
    pub async fn decide_latest(&self, decision: ApprovalDecision) -> Result<bool, Error> {
        let sid = self.session_id();
        let latest = self
            .sh
            .store
            .pending_approvals(&sid)?
            .into_iter()
            .max_by_key(|a| a.created_ms);
        match latest {
            Some(a) => {
                self.decide(a.id.as_str(), decision).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    // ---- scheduler (SEED.md §7.2, Phase 3) ------------------------------

    /// `/remind <N(s|m|h|d)> <text>`: create a reminder.
    pub fn add_reminder(&self, text: &str) -> Result<Option<ka_agent_core::TaskId>, Error> {
        let (duration_str, reminder_text) = text.split_once(' ').unwrap_or(("", ""));
        let ms = parse_duration(duration_str).ok_or_else(|| Error::Illegal {
            from: "remind".into(),
            to: format!("bad duration {duration_str:?}; use Ns, Nm, Nh or Nd"),
        })?;
        if reminder_text.is_empty() {
            return Ok(None);
        }
        let mut task = ka_agent_core::Task::new(format!("⏰ {reminder_text}"), "reminder", false);
        task.scheduled_at_ms = Some(ka_agent_core::now_ms() + ms);
        self.sh.store.insert_task(&task)?;
        Ok(Some(task.id))
    }

    /// Fire due reminders; called from the scheduler timer.
    pub async fn fire_due_reminders(&self) -> Result<Vec<String>, Error> {
        let now = ka_agent_core::now_ms();
        let due = self.sh.store.due_tasks(now, "reminder")?;
        let mut fired = Vec::new();
        for task in due {
            self.sh.store.set_task_status(&task.id, "running")?;
            let title = task.title.clone();
            self.sh.surface.on_note(&format!("⏰ {title}"));
            self.handle_message(&format!("⏰ reminder: {title}"), "scheduler")
                .await?;
            self.sh.store.set_task_status(&task.id, "succeeded")?;
            fired.push(title);
        }
        Ok(fired)
    }

    /// Scheduler loop; run it next to the expiry timer.
    pub async fn run_scheduler(&self, every: Duration) {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = self.fire_due_reminders().await {
                self.sh
                    .surface
                    .on_error("internal", &format!("scheduler: {e}"));
            }
        }
    }
    fn live_port(&self) -> Option<Arc<PortSession>> {
        self.lock().live.as_ref().map(|l| Arc::clone(&l.port))
    }

    fn audit(&self, action: &str, subject: &str, detail: Option<String>) -> Result<(), Error> {
        let mut rec = AuditRecord::new("operator", action, subject, detail);
        self.sh.store.append_audit(&mut rec)?;
        Ok(())
    }
    // ---- submission ------------------------------------------------------

    async fn submit_text(&self, text: &str) -> Result<(), Error> {
        if let Err(e) = self.ensure_worker().await {
            // A dead worker at submit time behaves like an interruption:
            // surface it, never auto-retry (SEED.md §3.8).
            self.sh
                .surface
                .on_error("internal", &format!("worker start failed: {e}"));
            let mut g = self.lock();
            let from = g.session.status;
            g.session
                .transition(SessionStatus::Idle)
                .map_err(|t| illegal_session(from, t))?;
            return Ok(());
        }
        let (sid, port) = {
            let g = self.lock();
            let Some(live) = g.live.as_ref() else {
                return Ok(());
            };
            (g.session.id.clone(), Arc::clone(&live.port))
        };
        // Record the turn BEFORE submitting (gap G8: ka persists per cycle,
        // so Ka Agent's record is the durable one).
        let mut turn = Turn::new(sid, text.to_string());
        self.sh.store.insert_turn(&turn)?;
        turn.transition(TurnStatus::Submitted)
            .map_err(|t| illegal_turn(turn.status, t))?;
        self.sh.store.update_turn(&turn)?;
        match port.submit(text.to_string(), Vec::new(), Vec::new()).await {
            Ok(()) => {}
            Err(PortError::EngineClosed) => {
                turn.transition(TurnStatus::Interrupted)
                    .map_err(|t| illegal_turn(turn.status, t))?;
                self.sh.store.update_turn(&turn)?;
                rt_on_engine_closed(&self.sh, port).await;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        turn.transition(TurnStatus::Running)
            .map_err(|t| illegal_turn(turn.status, t))?;
        self.sh.store.update_turn(&turn)?;
        {
            let mut g = self.lock();
            if let Some(live) = g.live.as_mut() {
                live.current_turn = Some(turn.id.clone());
                live.live_text.clear();
            }
        }
        self.sh.surface.on_turn_started();
        Ok(())
    }

    /// Lazily (re)start the worker with backoff and the crash-loop breaker.
    /// Falls back to a fresh strand when the recorded strand file no longer
    /// exists (SEED.md §3.5).
    async fn ensure_worker(&self) -> Result<(), Error> {
        if self.lock().live.is_some() {
            return Ok(());
        }
        let wait = Backoff::new(
            self.sh.cfg.worker_restart.backoff_start_ms,
            self.sh.cfg.worker_restart.backoff_max_ms,
        )
        .delay(self.lock().restart_attempt);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        let (session, env, engine_layer_toml) = {
            let g = self.lock();
            let env = WorkerEnv {
                home: self.sh.paths.engine_home(),
                data_dir: self.sh.paths.ka_data(),
                state_dir: self.sh.paths.engine_state(),
                cache_dir: self.sh.paths.engine_cache(),
                agent_data_root: self.sh.paths.root().to_path_buf(),
            };
            let layer = self
                .sh
                .cfg
                .engine_layer_path()
                .and_then(|p| std::fs::read_to_string(p).ok());
            (g.session.clone(), env, layer)
        };
        let selector = match (&session.strand_path, session.strand_id.as_ref()) {
            (Some(p), Some(_)) if p.exists() => StrandSelector::Path(p.clone()),
            _ => StrandSelector::New,
        };
        let req = OpenRequest {
            workspace: session.workspace.clone(),
            strand: selector,
            model: self.sh.cfg.model.clone(),
            mode: Some(self.sh.cfg.mode.clone()),
            engine_layer_toml,
            env,
            binary: self.sh.port.binary.clone(),
            worker_args: vec!["__worker".to_string()],
            scenario: None,
        };
        let opened = match self.sh.port.open(req.clone()).await {
            Ok(p) => p,
            Err(PortError::EngineClosed) if session.strand_path.is_some() => {
                // Recorded strand never materialized: start fresh.
                let mut retry = req;
                retry.strand = StrandSelector::New;
                self.sh.port.open(retry).await?
            }
            Err(e) => return Err(e.into()),
        };
        let port = Arc::new(opened);
        let rx = port.take_events();
        {
            let mut g = self.lock();
            g.restart_attempt = 0;
            g.live = Some(Live {
                port: Arc::clone(&port),
                pending_asks: HashMap::new(),
                live_text: String::new(),
                current_turn: None,
            });
        }
        tokio::spawn(drive(Arc::clone(&self.sh), port, rx));
        Ok(())
    }
}

/// The per-worker event loop: maps port events onto state, store and surface
/// (SEED.md §3.7: only events after our own submission are live output; the
/// delivery boundary is `idle`).
async fn drive(sh: Arc<Shared>, port: Arc<PortSession>, mut rx: mpsc::Receiver<PortEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            PortEvent::Bootstrap {
                strand_id,
                strand_path,
                replay_messages,
            } => {
                let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(id) = strand_id {
                    g.session.strand_id = Some(id);
                }
                if let Some(p) = strand_path {
                    g.session.strand_path = Some(std::path::PathBuf::from(p));
                }
                g.session.updated_ms = ka_agent_core::now_ms();
                let snapshot = g.session.clone();
                drop(g);
                if let (Some(sid_ref), Some(path_ref)) =
                    (&snapshot.strand_id, &snapshot.strand_path)
                {
                    if let Err(e) = sh.store.set_strand(&snapshot.id, sid_ref, path_ref) {
                        sh.surface
                            .on_error("internal", &format!("strand persist: {e}"));
                    }
                }
                if replay_messages > 0 {
                    sh.surface.on_note(&format!(
                        "resumed session: {replay_messages} replayed messages (not re-delivered)"
                    ));
                }
            }
            PortEvent::TurnStarted => sh.surface.on_turn_started(),
            PortEvent::DeltaText(t) => {
                let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(live) = g.live.as_mut() {
                    live.live_text.push_str(&t);
                }
                drop(g);
                sh.surface.on_delta_text(&t);
            }
            PortEvent::DeltaThought(t) => sh.surface.on_delta_thought(&t),
            PortEvent::CallStarted { tool, detail, .. } => {
                sh.surface.on_note(&format!("▸ {tool} {detail}"));
            }
            PortEvent::CallFinished {
                tool, ok: false, ..
            } => {
                sh.surface.on_note(&format!("✗ {tool} failed"));
            }
            PortEvent::CallFinished { ok: true, .. } => {}
            PortEvent::CallOutput {
                is_error: true,
                excerpt,
                ..
            } => {
                let first = excerpt.lines().next().unwrap_or("");
                sh.surface.on_note(&format!("  error: {first}"));
            }
            PortEvent::CallOutput {
                is_error: false, ..
            } => {}
            PortEvent::Ask {
                ask_id,
                question,
                detail,
                options,
            } => {
                let (sid, turn_id, ttl) = {
                    let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    (
                        g.session.id.clone(),
                        g.live
                            .as_ref()
                            .and_then(|l| l.current_turn.clone())
                            .unwrap_or_else(TurnId::generate),
                        sh.cfg.approval_ttl_secs,
                    )
                };
                let kind = ApprovalKind::KaAsk {
                    ask_id: ask_id.clone(),
                    question: question.clone(),
                    detail: detail.clone(),
                    options: options.clone(),
                };
                let approval = Approval::new(sid, turn_id, kind, ttl as i64);
                let card = ApprovalCard::from_approval(&approval);
                if let Err(e) = sh.store.insert_approval(&approval) {
                    sh.surface
                        .on_error("internal", &format!("approval persist: {e}"));
                }
                {
                    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let from = g.session.status;
                    g.session
                        .transition(SessionStatus::AwaitingApproval)
                        .map_err(|t| illegal_session(from, t))
                        .ok();
                    if let Some(live) = g.live.as_mut() {
                        live.pending_asks
                            .insert(approval.id.clone(), ask_id.clone());
                    }
                    let status = g.session.status;
                    drop(g);
                    rt_persist_status(&sh, status);
                }
                if let Some(card) = card {
                    sh.surface.on_approval(&card);
                }
            }
            PortEvent::TurnFinished {
                stop,
                cost_usd,
                tokens,
            } => {
                let turn_result = {
                    let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.live.as_ref().and_then(|l| l.current_turn.clone())
                };
                if let Some(tid) = turn_result {
                    if let Some(mut t) = sh.store.get_turn(&tid).ok().flatten() {
                        t.stop = Some(stop.clone());
                        t.cost_usd = cost_usd;
                        t.tokens = tokens;
                        let next = match stop.as_str() {
                            "done" | "length" => Some(TurnStatus::Done),
                            "error" => Some(TurnStatus::Error),
                            "aborted" => Some(TurnStatus::Aborted),
                            _ => None,
                        };
                        if let Some(next) = next {
                            t.transition(next)
                                .map_err(|x| illegal_turn(t.status, x))
                                .ok();
                        }
                        if let Err(e) = sh.store.update_turn(&t) {
                            sh.surface
                                .on_error("internal", &format!("turn persist: {e}"));
                        }
                    }
                }
                let (verdict, budget) = {
                    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let now = ka_agent_core::now_ms();
                    g.budget.record(cost_usd, tokens, now);
                    let b = g.budget.clone();
                    (
                        g.budget.verdict(),
                        serde_json::to_string(&b).unwrap_or_default(),
                    )
                };
                sh.store.meta_set("budget", &budget).ok();
                sh.surface.on_turn_finished(&stop, cost_usd, tokens);
                if verdict == BudgetVerdict::Exceeded {
                    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let from = g.session.status;
                    g.session
                        .transition(SessionStatus::Paused)
                        .map_err(|t| illegal_session(from, t))
                        .ok();
                    let status = g.session.status;
                    drop(g);
                    rt_persist_status(&sh, status);
                    sh.surface
                        .on_note("daily budget reached — paused until tomorrow (override via CLI)");
                }
                if stop == "error" {
                    sh.surface
                        .on_note("turn ended in error; see /status — nothing was re-submitted");
                }
            }
            PortEvent::Idle => {
                // Delivery boundary (SEED.md §3.7): flush the buffered final
                // answer through the outbox, then drain the durable queue.
                let (text, turn_id, sid) = {
                    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let mut text = String::new();
                    let mut turn_id = None;
                    if let Some(live) = g.live.as_mut() {
                        text = std::mem::take(&mut live.live_text);
                        turn_id = live.current_turn.clone();
                    }
                    (text, turn_id, g.session.id.clone())
                };
                if let Some(tid) = &turn_id {
                    let parts = split_final_text(&text, FINAL_CHUNK_MAX);
                    for (i, chunk) in parts.iter().enumerate() {
                        let mut oc = OutboxChunk::new(tid.clone(), i as u32, chunk.clone());
                        sh.store.push_chunk(&oc).ok();
                        if parts.len() > 1 {
                            sh.surface.on_final_chunk(i as u32 + 1, chunk);
                        } else {
                            sh.surface.on_final_chunk(1, chunk);
                        }
                        oc.status = ka_agent_core::OutboxStatus::Sent;
                        oc.channel_message_id = Some(i as i64);
                        sh.store.mark_sent(&oc.turn, oc.chunk_index, i as i64).ok();
                    }
                }
                // Episodic index (SEED.md §4.5): one user and one assistant
                // row per prompt cycle, at the delivery boundary.
                if let Some(tid) = &turn_id {
                    let inbound = sh
                        .store
                        .get_turn(tid)
                        .ok()
                        .flatten()
                        .map(|t| t.inbound_text)
                        .unwrap_or_default();
                    sh.store.index_episode(&sid, tid, "user", &inbound).ok();
                    sh.store.index_episode(&sid, tid, "assistant", &text).ok();
                }
                let drained = drain_queue(&sh).await;
                if !drained {
                    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    if g.session.status != SessionStatus::Idle {
                        let from = g.session.status;
                        g.session
                            .transition(SessionStatus::Idle)
                            .map_err(|t| illegal_session(from, t))
                            .ok();
                        let status = g.session.status;
                        drop(g);
                        rt_persist_status(&sh, status);
                        sh.surface.on_session_status(status);
                    }
                }
            }
            PortEvent::Note(t) => sh.surface.on_note(&t),
            PortEvent::EngineError { class, message, .. } => {
                sh.surface.on_error(&class, &message);
                if class == "auth" {
                    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let from = g.session.status;
                    g.session
                        .transition(SessionStatus::Paused)
                        .map_err(|t| illegal_session(from, t))
                        .ok();
                    let status = g.session.status;
                    drop(g);
                    rt_persist_status(&sh, status);
                    sh.surface
                        .on_note("auth failure — paused; fix the provider key and /resume");
                }
            }
            PortEvent::EngineClosed => {
                let interrupted = {
                    let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.live.as_ref().and_then(|l| l.current_turn.clone())
                };
                if let Some(tid) = interrupted {
                    if let Some(mut t) = sh.store.get_turn(&tid).ok().flatten() {
                        if !t.status.is_terminal() {
                            t.transition(TurnStatus::Interrupted)
                                .map_err(|x| illegal_turn(t.status, x))
                                .ok();
                            sh.store.update_turn(&t).ok();
                        }
                    }
                }
                rt_on_engine_closed(&sh, port.clone()).await;
            }
        }
    }
}

/// Submit the next queued message, if any. Returns true when a new cycle is
/// running (session stays `running`).
async fn drain_queue(sh: &Arc<Shared>) -> bool {
    let sid = {
        sh.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session
            .id
            .clone()
    };
    let Some(item) = sh.store.next_queued(&sid).ok().flatten() else {
        return false;
    };
    let port = {
        let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.live.as_ref().map(|l| Arc::clone(&l.port))
    };
    let Some(port) = port else {
        return false;
    };
    let mut turn = Turn::new(sid.clone(), item.text.clone());
    if let Err(e) = sh.store.insert_turn(&turn) {
        sh.surface
            .on_error("internal", &format!("queue drain: {e}"));
        return false;
    }
    if turn.transition(TurnStatus::Submitted).is_err() {
        return false;
    }
    sh.store.update_turn(&turn).ok();
    sh.store.mark_turned(&item.id, &turn.id).ok();
    match port.submit(item.text.clone(), Vec::new(), Vec::new()).await {
        Ok(()) => {
            turn.transition(TurnStatus::Running)
                .map_err(|t| illegal_turn(turn.status, t))
                .ok();
            sh.store.update_turn(&turn).ok();
            let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(live) = g.live.as_mut() {
                live.current_turn = Some(turn.id.clone());
                live.live_text.clear();
            }
            true
        }
        Err(_) => {
            turn.transition(TurnStatus::Interrupted)
                .map_err(|t| illegal_turn(turn.status, t))
                .ok();
            sh.store.update_turn(&turn).ok();
            false
        }
    }
}

fn rt_persist_status(sh: &Arc<Shared>, status: SessionStatus) {
    let sid = sh
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .session
        .id
        .clone();
    if let Err(e) = sh.store.update_session_status(&sid, status) {
        sh.surface
            .on_error("internal", &format!("session status persist: {e}"));
    }
}

async fn rt_on_engine_closed(sh: &Arc<Shared>, port: Arc<PortSession>) {
    port.kill().await;
    let now = ka_agent_core::now_ms();
    let window_ms = (sh.cfg.worker_restart.crash_window_secs as i64) * 1000;
    let (status, note) = {
        let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.live = None;
        g.crashes.retain(|t| now - *t < window_ms);
        g.crashes.push(now);
        g.restart_attempt = g.restart_attempt.saturating_add(1);
        let degraded = g.crashes.len() >= sh.cfg.worker_restart.crash_max as usize;
        let from = g.session.status;
        let target = if degraded {
            SessionStatus::Degraded
        } else {
            SessionStatus::Idle
        };
        let note = if degraded {
            "engine closed repeatedly — session degraded; /new to recover".to_string()
        } else {
            "engine closed; the next message restarts the worker".to_string()
        };
        g.session
            .transition(target)
            .map_err(|t| illegal_session(from, t))
            .ok();
        (target, note)
    };
    for id in sh.store.expire_pending(now).unwrap_or_default() {
        sh.surface
            .on_approval_settled(id.as_str(), ka_agent_core::ApprovalStatus::Expired);
    }
    rt_persist_status(sh, status);
    sh.surface.on_session_status(status);
    sh.surface.on_note(&note);
}

/// Parse `Ns`, `Nm`, `Nh`, `Nd` into milliseconds.
fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 2 {
        return None;
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: i64 = num.parse().ok()?;
    match unit {
        "s" => Some(n * 1000),
        "m" => Some(n * 60_000),
        "h" => Some(n * 3_600_000),
        "d" => Some(n * 86_400_000),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn backoff_starts_at_start_and_caps() {
        let b = Backoff::new(1000, 60_000);
        assert_eq!(b.delay(0), Duration::from_millis(1000));
        assert_eq!(b.delay(1), Duration::from_millis(2000));
        assert_eq!(b.delay(6), Duration::from_millis(60_000));
        assert_eq!(b.delay(20), Duration::from_millis(60_000));
    }

    #[test]
    fn backoff_min_one_ms() {
        let b = Backoff::new(0, 0);
        assert_eq!(b.delay(0), Duration::from_millis(1));
    }

    #[test]
    fn split_respects_paragraphs_and_cap() {
        let text = "a\n\nb\n\nc";
        assert_eq!(split_final_text(text, 10), vec![text.to_string()]);
        let long = format!("{}\n\n{}", "x".repeat(30), "y".repeat(30));
        let parts = split_final_text(&long, 30);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].chars().count(), 30);
        let one = "z".repeat(100);
        let parts = split_final_text(&one, 30);
        assert_eq!(parts.len(), 4);
        assert!(parts.iter().all(|p| p.chars().count() <= 30));
    }

    #[test]
    fn split_never_returns_empty_vec() {
        assert_eq!(split_final_text("", 10), vec![String::new()]);
    }
}
