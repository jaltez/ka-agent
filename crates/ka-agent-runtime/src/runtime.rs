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
    Approval, ApprovalDecision, ApprovalId, ApprovalKind, ApprovalStatus, AuditRecord, BudgetState,
    BudgetVerdict, ConfigError, DaemonConfig, OutboxChunk, QueueItem, Session, SessionId,
    SessionStatus, Turn, TurnId, TurnStatus,
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
    #[error("{0}")]
    Refused(String),
    #[error("skills: {0}")]
    Skills(#[from] crate::skills::Error),
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
    /// We killed this worker on purpose (`/new`, the `/stop` escalation): its
    /// exit must not be booked as a crash.
    deliberate_close: bool,
    /// Last time anything happened on this session (event, submit, ask) —
    /// the idle reap's clock. `now_ms` while a turn is in flight.
    last_activity_ms: i64,
}

struct Inner {
    session: Session,
    live: Option<Live>,
    budget: BudgetState,
    /// Timestamps (ms) of recent worker crashes, for the crash-loop breaker.
    crashes: Vec<i64>,
    restart_attempt: u32,
}
/// A no-agent cron handler: input is the spec text, output the digest text.
pub type NoAgentFn = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

struct Shared {
    /// The live configuration; swapped whole by [`Runtime::reload_config`].
    cfg: std::sync::RwLock<Arc<DaemonConfig>>,
    /// The file the configuration came from (for `/model` and reloads).
    config_path: Mutex<Option<std::path::PathBuf>>,
    paths: Paths,
    store: Arc<Store>,
    memory: ka_agent_memory::MemoryStore,
    port: WorkerPort,
    surface: Arc<dyn Surface>,
    inner: Mutex<Inner>,
    /// No-agent cron handlers, registered by kind (e.g. "calendar").
    /// Registered by the app binary; the runtime only dispatches (M11).
    no_agent: Mutex<HashMap<String, NoAgentFn>>,
    /// Wakes the scheduler when a task is scheduled sooner than its sleep.
    scheduler_tick: tokio::sync::Notify,
}

fn cfg_of(sh: &Shared) -> Arc<DaemonConfig> {
    Arc::clone(&sh.cfg.read().unwrap_or_else(|e| e.into_inner()))
}

/// Field comparison for config sections that only derive `Debug`.
fn differs<T: std::fmt::Debug>(a: &T, b: &T) -> bool {
    format!("{a:?}") != format!("{b:?}")
}

/// What a config reload changed: applied to the running agent, or waiting
/// for a restart.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ReloadReport {
    pub applied: Vec<String>,
    pub restart_required: Vec<String>,
}

/// Whether the engine can resolve the configured model's provider key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelKeyState {
    Present,
    Missing(String),
    Keyless,
}

/// What `/remind rm` or `/cron rm` cancelled.
#[derive(Debug, Clone)]
pub enum CancelOutcome {
    Cancelled(String),
    NotFound,
    Ambiguous(Vec<String>),
}

/// The runtime handle. Cheap to clone; the event loop and the expiry timer
/// share the same state.
#[derive(Clone)]
pub struct Runtime {
    sh: Arc<Shared>,
}

/// The memory preamble tells the model which zone to schedule in: every
/// clock time the operator writes is local, never UTC.
pub fn memory_preamble(cfg: &DaemonConfig) -> String {
    let tz = cfg.time_zone();
    let offset = jiff::Timestamp::now().to_zoned(tz.clone()).offset();
    format!(
        "Operator timezone: {} (UTC{} now). Schedule clock times in this zone.",
        tz.iana_name().unwrap_or("UTC"),
        offset
    )
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
            surface.on_notice(&format!(
                "recovered after restart: {} interrupted turn(s) marked; nothing was re-submitted",
                interrupted.len()
            ));
        }
        // A crash mid-fire leaves scheduled tasks `running`; without this
        // they sit invisible to the scheduler forever while `/cron list`
        // still shows them. Marked `interrupted` (= paused, resumable).
        let stuck_tasks = store.reconcile_running_tasks(ka_agent_core::now_ms())?;
        if !stuck_tasks.is_empty() {
            surface.on_notice(&format!(
                "recovered after restart: {} scheduled task(s) interrupted mid-fire marked paused",
                stuck_tasks.len()
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
            // `new_session` audits this; the daemon's own bootstrap session
            // must land in the chain too, or the log starts with a session
            // nothing explains (SEED.md §8.7).
            let mut rec = AuditRecord::new(
                "system",
                audit_actions::SESSION_NEW,
                session.id.as_str(),
                Some("daemon bootstrap".into()),
            );
            store.append_audit(&mut rec)?;
        }
        // An adopted session can carry a status from a previous process whose
        // turn died with it (`running`, `awaiting_approval`, `stopping`):
        // those states make `handle_message` queue instead of submit, and the
        // queue only drains from a worker's `idle` — so the daemon would look
        // alive and answer nothing, forever (trace: crash, `systemctl stop`,
        // `ka-agent restart` mid-turn). Reconciliation marked the turns
        // interrupted above; the status follows.
        let resumable = [
            ka_agent_core::SessionStatus::Running,
            ka_agent_core::SessionStatus::AwaitingApproval,
            ka_agent_core::SessionStatus::Stopping,
        ];
        if resumable.contains(&session.status) {
            let from = session.status;
            if session
                .transition(ka_agent_core::SessionStatus::Idle)
                .is_ok()
            {
                store.update_session_status(&session.id, session.status)?;
                surface.on_notice(&format!(
                    "recovered session status {from:?} → idle (its turn died with the previous process)"
                ));
            }
        }
        let session = session;
        let mut budget = BudgetState::new(cfg.budgets.daily_usd, cfg.budgets.daily_tokens);
        if let Some(saved) = store.meta_get("budget")?
            && let Ok(b) = serde_json::from_str::<BudgetState>(&saved)
            && b.day == ka_agent_core::day_key(ka_agent_core::now_ms())
        {
            budget = b;
            // The saved window keeps today's spend; the caps always
            // come from the current config.
            budget.daily_usd_cap = cfg.budgets.daily_usd;
            budget.daily_token_cap = cfg.budgets.daily_tokens;
        }
        let memory = ka_agent_memory::MemoryStore::open(&paths.root().join("memory"))?;
        // Compile stable memory into the engine's user-tier MEMORY.md; ka
        // re-reads it per prompt (validation §5), so no worker restart is
        // needed after updates.
        memory.install_compiled(
            &paths.engine_home().join(".config/ka"),
            ka_agent_memory::DEFAULT_BUDGET_BYTES,
            &memory_preamble(&cfg),
        )?;
        // Operator-facing workspace context (M11): never overwrite an
        // existing AGENTS.md; a read-only workspace must not fail startup.
        if let Err(e) = write_agents_md(&cfg) {
            eprintln!("runtime: AGENTS.md not written: {e}");
        }
        Ok(Self {
            sh: Arc::new(Shared {
                cfg: std::sync::RwLock::new(Arc::new(cfg)),
                config_path: Mutex::new(None),
                paths,
                store,
                memory,
                port,
                surface,
                no_agent: Mutex::new(HashMap::new()),
                scheduler_tick: tokio::sync::Notify::new(),
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

    /// A snapshot of the live configuration.
    pub fn config(&self) -> Arc<DaemonConfig> {
        cfg_of(&self.sh)
    }

    /// Remember the config file this runtime was built from.
    pub fn set_config_path(&self, path: std::path::PathBuf) {
        *self
            .sh
            .config_path
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(path);
    }

    pub fn config_path(&self) -> Option<std::path::PathBuf> {
        self.sh
            .config_path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn cfg(&self) -> Arc<DaemonConfig> {
        cfg_of(&self.sh)
    }

    /// The operator's workspace: where `@` file references resolve.
    pub fn workspace(&self) -> std::path::PathBuf {
        self.cfg().workspace_path()
    }

    /// Now, in the operator's zone (configured, else the system zone).
    pub fn now_zoned(&self) -> jiff::Zoned {
        jiff::Timestamp::now().to_zoned(self.cfg().time_zone())
    }

    /// The current session status.
    pub fn session_status(&self) -> SessionStatus {
        self.lock().session.status
    }

    /// Everything `/status` shows, in one struct.
    pub fn status_report(&self) -> Result<crate::status::StatusReport, Error> {
        let cfg = self.cfg();
        let (sid, status, strand, worker_running, budget, crashes) = {
            let g = self.lock();
            (
                g.session.id.clone(),
                g.session.status,
                g.session.strand_id.clone(),
                g.live.is_some(),
                g.budget.clone(),
                g.crashes.len(),
            )
        };
        let queue = self.sh.store.queue_len(&sid)?;
        let pending_approvals = self.sh.store.pending_approvals(&sid)?.len();
        let next_task = self
            .sh
            .store
            .scheduled_tasks()?
            .into_iter()
            .find(|t| t.status == ka_agent_core::TaskStatus::Queued)
            .and_then(|t| t.scheduled_at_ms.map(|at| (t.title, at)));
        let tz = cfg.time_zone();
        Ok(crate::status::StatusReport {
            session_id: sid.to_string(),
            status: format!("{status:?}"),
            strand_id: strand,
            model: cfg.model.clone(),
            mode: cfg.mode.clone(),
            worker_running,
            idle_timeout_secs: cfg.worker_idle_timeout_secs,
            spent_usd: budget.spent_usd,
            spent_tokens: budget.spent_tokens,
            limit_usd: cfg.budgets.daily_usd,
            limit_tokens: cfg.budgets.daily_tokens,
            queue,
            pending_approvals,
            crashes,
            next_task,
            timezone: tz.iana_name().unwrap_or("UTC").to_string(),
        })
    }

    /// Swap in a new configuration. Settings the runtime reads per use apply
    /// immediately; channel, workspace and web settings need a restart. A
    /// changed model/mode/engine layer (or `restart_worker`, after a key
    /// change) stops an idle worker so the next message starts a fresh one
    /// with the new settings; a running turn finishes on the old ones.
    pub async fn reload_config(&self, new: DaemonConfig, restart_worker: bool) -> ReloadReport {
        let old = self.cfg();
        let live_fields = [
            ("model", differs(&old.model, &new.model)),
            ("mode", differs(&old.mode, &new.mode)),
            (
                "ka.engine_layer",
                differs(&old.ka.engine_layer, &new.ka.engine_layer),
            ),
            (
                "approval_ttl_secs",
                differs(&old.approval_ttl_secs, &new.approval_ttl_secs),
            ),
            (
                "turn_grace_secs",
                differs(&old.turn_grace_secs, &new.turn_grace_secs),
            ),
            (
                "worker_idle_timeout_secs",
                differs(&old.worker_idle_timeout_secs, &new.worker_idle_timeout_secs),
            ),
            (
                "worker_restart",
                differs(&old.worker_restart, &new.worker_restart),
            ),
            ("budgets", differs(&old.budgets, &new.budgets)),
            ("timezone", differs(&old.timezone, &new.timezone)),
            ("calendar", differs(&old.calendar, &new.calendar)),
        ];
        let restart_fields = [
            ("workspace", differs(&old.workspace, &new.workspace)),
            ("telegram", differs(&old.telegram, &new.telegram)),
            ("matrix", differs(&old.matrix, &new.matrix)),
            ("web", differs(&old.web, &new.web)),
        ];
        let names = |fields: &[(&str, bool)]| -> Vec<String> {
            fields
                .iter()
                .filter(|(_, c)| *c)
                .map(|(n, _)| n.to_string())
                .collect()
        };
        let report = ReloadReport {
            applied: names(&live_fields),
            restart_required: names(&restart_fields),
        };
        let engine_changed = live_fields[..3].iter().any(|(_, c)| *c);
        {
            let mut g = self.lock();
            g.budget.daily_usd_cap = new.budgets.daily_usd;
            g.budget.daily_token_cap = new.budgets.daily_tokens;
        }
        *self.sh.cfg.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(new);
        if restart_worker || engine_changed {
            self.stop_idle_worker().await;
        }
        report
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.sh.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- operator operations -------------------------------------------

    /// `/new`: drop the live worker and start a fresh session/strand.
    pub async fn new_session(&self) -> Result<SessionId, Error> {
        // `/new` kills the worker on purpose: its exit is not a crash, and a
        // fresh session starts with a clean crash history (five `/new` in ten
        // minutes used to degrade the brand-new session).
        {
            let mut g = self.lock();
            if let Some(live) = g.live.as_mut() {
                live.deliberate_close = true;
            }
            g.crashes.clear();
        }
        let live_port = {
            let mut g = self.lock();
            g.live.take().map(|l| l.port)
        };
        if let Some(port) = live_port {
            port.kill().await;
        }
        let session = {
            let mut g = self.lock();
            g.session = Session::new(self.cfg().workspace_path());
            g.restart_attempt = 0;
            g.session.clone()
        };
        self.sh.store.insert_session(&session)?;
        self.audit(audit_actions::SESSION_NEW, session.id.as_str(), None)?;
        self.sh.surface.on_session_status(SessionStatus::Idle);
        Ok(session.id)
    }

    /// `/resume`: lift a pause (budget override or a fixed auth key) back to
    /// idle. Refuses when the budget verdict is still exceeded — the
    /// operator asked explicitly, so say why instead of silently ignoring.
    pub fn resume(&self) -> Result<bool, Error> {
        let mut g = self.lock();
        if g.session.status != SessionStatus::Paused {
            return Ok(false);
        }
        if g.budget.verdict() == ka_agent_core::BudgetVerdict::Exceeded {
            return Err(Error::Refused(
                "budget still exceeded for this window; raise it in ka-agent.toml or /new".into(),
            ));
        }
        let from = g.session.status;
        g.session
            .transition(SessionStatus::Idle)
            .map_err(|t| illegal_session(from, t))?;
        let status = g.session.status;
        drop(g);
        self.persist_session_status(status);
        self.audit(
            audit_actions::SESSION_RESUME,
            self.session_id().as_str(),
            None,
        )?;
        Ok(true)
    }

    /// Inbound message: submit now when idle, otherwise enqueue durably
    /// (gap G4 — ka drops mid-cycle prompts, HTTP 200 notwithstanding).
    pub async fn handle_message(&self, text: &str, source: &str) -> Result<(), Error> {
        self.sh.surface.on_inbound(source, text);
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
                    // A budget pause lifts itself when the day window rolls
                    // (the verdict is recomputed from the recorded window);
                    // an auth pause needs /resume, which is why the note
                    // says so honestly.
                    let verdict = {
                        let g = self.lock();
                        g.budget.verdict()
                    };
                    if verdict == ka_agent_core::BudgetVerdict::Exceeded {
                        self.sh.surface.on_notice(
                            "paused: budget reached for this window; use /resume to override or wait for the window to roll",
                        );
                        false
                    } else {
                        // From is always Paused here.
                        g.session
                            .transition(SessionStatus::Running)
                            .map_err(|t| illegal_session(SessionStatus::Paused, t))?;
                        drop(g);
                        self.persist_session_status(SessionStatus::Running);
                        true
                    }
                }
                SessionStatus::Degraded => {
                    self.sh.surface.on_notice(
                        "⚠ degraded — the worker keeps crashing; /new starts a fresh session",
                    );
                    false
                }
            }
        };
        if submit_now {
            // Persist immediately: the store is the durable source of truth
            // for recovery, and a row left at `idle` makes every later status
            // write look like an illegal transition.
            self.persist_session_status(SessionStatus::Running);
        }
        if !submit_now {
            match self.session_status() {
                SessionStatus::Running
                | SessionStatus::AwaitingApproval
                | SessionStatus::Stopping => {
                    let item = QueueItem::new(self.session_id(), text.to_string(), source);
                    self.sh.store.enqueue(&item)?;
                    let n = self.sh.store.queue_len(&self.session_id())?;
                    self.sh.surface.on_notice(&format!("queued (#{n})"));
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

    /// `/skill <name> <text>`: submit one turn with that skill's full body
    /// injected (SEED.md §2.6, §9.1). Fails closed: the skill must be
    /// provisioned in the engine home, and the session must be idle — a
    /// skill turn is never queued (the queue carries plain text only).
    pub async fn handle_skill(&self, name: &str, text: &str) -> Result<(), Error> {
        let skills = crate::skills::Skills::new(&self.sh.paths.engine_home());
        if !skills.is_enabled(name)? {
            return Err(Error::Refused(format!(
                "skill {name:?} is not enabled — `ka-agent skills enable {name}`"
            )));
        }
        if text.trim().is_empty() {
            return Err(Error::Refused("usage: /skill <name> <text>".into()));
        }
        {
            let mut g = self.lock();
            if g.session.status != SessionStatus::Idle {
                return Err(Error::Refused(
                    "session busy — /skill runs when idle; nothing was queued".into(),
                ));
            }
            let from = g.session.status;
            g.session
                .transition(SessionStatus::Running)
                .map_err(|t| illegal_session(from, t))?;
        }
        self.persist_session_status(SessionStatus::Running);
        match self.submit_text_with(text, vec![name.to_string()]).await {
            Ok(()) => Ok(()),
            Err(e) => {
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

    /// Skills the engine currently discovers (for `/skills` and doctor).
    pub fn skills_enabled(&self) -> Vec<crate::skills::SkillInfo> {
        crate::skills::Skills::new(&self.sh.paths.engine_home()).enabled()
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

    /// The raw text of the file an accepted candidate lands in (the web
    /// editor renders the diff against it).
    pub fn memory_facts_text(&self) -> Result<Option<String>, Error> {
        Ok(self
            .sh
            .memory
            .file_text(&ka_agent_memory::MemoryFile::Facts)?)
    }

    /// `/memory review`: pending candidates.
    pub fn pending_candidates(&self) -> Result<Vec<ka_agent_store::CandidateRow>, Error> {
        Ok(self.sh.store.pending_candidates_full()?)
    }

    /// `/memory accept <id>`: candidate → stable memory. The id may be a
    /// unique prefix (like `/cron rm`).
    pub fn accept_candidate(&self, id: &str) -> Result<bool, Error> {
        let pending = self.sh.store.pending_candidates_full()?;
        let matches: Vec<_> = pending
            .iter()
            .filter(|c| c.id.as_str() == id || c.id.as_str().starts_with(id))
            .collect();
        let Some(row) = (match matches.as_slice() {
            [one] => Some((**one).clone()),
            [] => None,
            _ => {
                return Err(Error::Refused(
                    "ambiguous candidate id: type more of it".into(),
                ));
            }
        }) else {
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

    /// `/memory reject <id>`: the same full-id-or-unique-prefix rule.
    pub fn reject_candidate(&self, id: &str) -> Result<bool, Error> {
        let pending = self.sh.store.pending_candidates_full()?;
        let matches: Vec<_> = pending
            .iter()
            .filter(|c| c.id.as_str() == id || c.id.as_str().starts_with(id))
            .collect();
        let row = match matches.as_slice() {
            [one] => (**one).clone(),
            [] => return Ok(false),
            _ => {
                return Err(Error::Refused(
                    "ambiguous candidate id: type more of it".into(),
                ));
            }
        };
        self.sh
            .store
            .set_candidate_status(&row.id.clone(), "rejected")?;
        self.audit(
            audit_actions::MEMORY_EDIT,
            row.id.as_str(),
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

    /// `/history show <turn|prefix>`: the stored exchange. The id may be
    /// the full id or a prefix (with or without the `turn_` family).
    pub fn history_show(&self, turn_id: &str) -> Result<Option<(String, String)>, Error> {
        let turn = match self
            .sh
            .store
            .get_turn(&ka_agent_core::TurnId(turn_id.to_string()))?
        {
            Some(t) => t,
            None => match self.sh.store.find_turn_by_prefix(turn_id.trim())? {
                Some(t) => t,
                None => return Ok(None),
            },
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
            &memory_preamble(&self.cfg()),
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
            self.sh.surface.on_notice("nothing to stop");
            return Ok(());
        };
        self.sh.surface.on_notice("stopping…");
        port.abort().await.ok();
        let grace = Duration::from_secs(self.cfg().turn_grace_secs);
        // Escalate on the *turn*, not the process: a successful abort leaves
        // the worker attached (it is reused for the next turn), and the old
        // `is_closed()` test therefore always killed it — SIGKILLing a turn
        // the abort had already drained into.
        let sh = Arc::clone(&self.sh);
        let turn = {
            let g = self.lock();
            g.live.as_ref().and_then(|l| l.current_turn.clone())
        };
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let turn_still_open = turn
                .as_ref()
                .and_then(|t| sh.store.get_turn(t).ok().flatten())
                .is_some_and(|t| !t.status.is_terminal());
            if turn_still_open && !port.is_closed() {
                if let Some(live) = sh
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .live
                    .as_mut()
                {
                    live.deliberate_close = true;
                }
                port.kill().await;
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
        // Deny-on-expiry is an invariant, not a schedule: a decision that
        // arrives between the TTL and the next sweep must not allow the ask.
        if let Ok(row) = self.sh.store.get_approval(&id)
            && row.status == ka_agent_core::ApprovalStatus::Pending
            && ka_agent_core::now_ms() >= row.expires_at_ms
        {
            return Err(Error::UnknownApproval(approval_id.to_string()));
        }
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
        // The engine has its deny; the ask is settled. Leaving it in the map
        // would keep `ask_pending` true forever, which blocks the idle
        // transition and the queue drain — every later message would queue
        // with no engine left to consume it.
        {
            let mut g = self.lock();
            if let Some(live) = g.live.as_mut() {
                for id in &expired {
                    live.pending_asks.remove(id);
                }
            }
        }
        // An expired ask while the session sat in `awaiting_approval` is the
        // end of that wait: with nothing else pending the turn must settle.
        rt_maybe_idle(&self.sh);
        Ok(expired)
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

    // ---- scheduler (SEED.md §7.2; generalized cron M11) -----------------

    /// Schedule a reminder at an absolute time (parsing is the router's
    /// job: every surface shares one grammar).
    pub fn add_reminder_at(&self, at_ms: i64, text: &str) -> Result<ka_agent_core::TaskId, Error> {
        if text.trim().is_empty() {
            return Err(Error::Refused("a reminder needs a text".into()));
        }
        let mut task = ka_agent_core::Task::new(text.trim().to_string(), "reminder", false);
        task.scheduled_at_ms = Some(at_ms);
        self.sh.store.insert_task(&task)?;
        self.sh.scheduler_tick.notify_one();
        Ok(task.id)
    }

    /// The model's provider key state against the engine `.env` (the
    /// same file the worker reads): `None` when no model is configured.
    pub fn model_key_state(&self) -> Result<Option<(String, ModelKeyState)>, Error> {
        let Some(model) = self.cfg().model.clone() else {
            return Ok(None);
        };
        let sel = ka_agent_port::catalog::validate_selector(&model).map_err(Error::Refused)?;
        let env = ka_agent_port::catalog::read_env_file(&ka_agent_port::catalog::engine_env_path(
            &self.sh.paths.engine_home(),
        ));
        let state = match ka_agent_port::catalog::key_state(&sel, &env) {
            ka_agent_port::catalog::KeyState::Present => ModelKeyState::Present,
            ka_agent_port::catalog::KeyState::Missing(name) => ModelKeyState::Missing(name),
            ka_agent_port::catalog::KeyState::Keyless => ModelKeyState::Keyless,
        };
        Ok(Some((sel.id, state)))
    }

    /// `/model <provider/model>`: validate, save to the config file, apply
    /// live. Returns the normalized selector for the reply.
    pub async fn set_model(&self, sel: &str) -> Result<String, Error> {
        let info = ka_agent_port::catalog::validate_selector(sel).map_err(Error::Refused)?;
        let path = self
            .config_path()
            .ok_or_else(|| Error::Refused("no config file — run ka-agent setup".into()))?;
        ka_agent_core::config_file::set(&path, "model", &info.id).map_err(Error::Config)?;
        let new = DaemonConfig::load(&path)?;
        self.reload_config(new, false).await;
        Ok(info.id)
    }

    /// The `id8` history line listing: the session's last `limit` prompt
    /// cycles with their answers.
    pub fn recent_turns(&self, limit: usize) -> Result<Vec<(ka_agent_core::Turn, String)>, Error> {
        let sid = self.session_id();
        let turns = self.sh.store.latest_turns_for_session(&sid, limit)?;
        let mut out = Vec::new();
        for t in turns {
            let answer: String = self
                .sh
                .store
                .chunks_for_turn(&t.id)?
                .into_iter()
                .map(|c| c.text)
                .collect();
            out.push((t, answer));
        }
        Ok(out)
    }

    /// Pending approval cards of the live session (`/approvals`, `/y <id8>`).
    pub fn pending_approval_cards(&self) -> Result<Vec<crate::surface::ApprovalCard>, Error> {
        let sid = self.session_id();
        Ok(self
            .sh
            .store
            .pending_approvals(&sid)?
            .iter()
            .filter_map(crate::surface::ApprovalCard::from_approval)
            .collect())
    }

    /// Register a no-agent cron handler by kind (e.g. "calendar"). The app
    /// binary does this at startup; handlers run on the scheduler tick and
    /// never touch the engine.
    pub fn set_no_agent_handler(&self, kind: &str, f: NoAgentFn) {
        let mut reg = self.sh.no_agent.lock().unwrap_or_else(|e| e.into_inner());
        reg.insert(kind.to_string(), f);
    }

    /// Dispatch a no-agent cron handler by kind.
    pub fn run_no_agent(&self, kind: &str, arg: &str) -> Result<String, Error> {
        let handler = {
            let reg = self.sh.no_agent.lock().unwrap_or_else(|e| e.into_inner());
            reg.get(kind).cloned()
        };
        match handler {
            Some(f) => f(arg).map_err(Error::Refused),
            None => Err(Error::Refused(format!("no handler for kind {kind:?}"))),
        }
    }

    /// Stop a worker that has had nothing to do for
    /// `worker_idle_timeout_secs`. Minimal-footprint rule: an idle resident
    /// must not pay for a resident engine. The kill is `deliberate_close`
    /// — not a crash, no backoff, no degradation — and the next message
    /// restarts the worker, resuming the strand by path. Skipped unless
    /// the session is fully settled: idle status, no open turn, no pending
    /// ask, nothing queued.
    async fn reap_idle_worker(&self) {
        let timeout_secs = self.cfg().worker_idle_timeout_secs;
        if timeout_secs == 0 {
            return;
        }
        self.stop_settled_worker((timeout_secs as i64) * 1000).await;
    }

    /// Stop the worker now if the session is settled (config reload, key
    /// change): the next message starts a fresh worker with new settings.
    /// Returns whether a worker was stopped.
    pub async fn stop_idle_worker(&self) -> bool {
        self.stop_settled_worker(0).await
    }

    /// Deliberately kill a settled worker that has been inactive for at
    /// least `min_idle_ms`.
    async fn stop_settled_worker(&self, min_idle_ms: i64) -> bool {
        let now = ka_agent_core::now_ms();
        let port = {
            let mut g = self.lock();
            let (settled, idle_long_enough, port) = {
                let Some(live) = g.live.as_ref() else {
                    return false;
                };
                let turn_open = live.current_turn.as_ref().is_some_and(|t| {
                    self.sh
                        .store
                        .get_turn(t)
                        .ok()
                        .flatten()
                        .is_some_and(|t| !t.status.is_terminal())
                });
                let settled = g.session.status == SessionStatus::Idle
                    && !turn_open
                    && live.pending_asks.is_empty()
                    && self.sh.store.queue_len(&g.session.id).unwrap_or(0) == 0;
                let idle_long_enough = now - live.last_activity_ms >= min_idle_ms;
                (settled, idle_long_enough, live.port.clone())
            };
            if !settled || !idle_long_enough {
                return false;
            }
            if let Some(live) = g.live.as_mut() {
                live.deliberate_close = true;
            }
            port
        };
        port.kill().await;
        true
    }

    /// Schedule a cron job: a `kind = "cron"` task carrying its
    /// [`ka_agent_core::CronSpec`] as payload, first firing at
    /// `first_at_ms`.
    pub fn add_cron(
        &self,
        spec: ka_agent_core::CronSpec,
        first_at_ms: i64,
    ) -> Result<ka_agent_core::TaskId, Error> {
        let mut task = ka_agent_core::Task::new(spec.text.clone(), "cron", false);
        task.scheduled_at_ms = Some(first_at_ms);
        task.payload = Some(spec.to_payload());
        self.sh.store.insert_task(&task)?;
        self.sh.scheduler_tick.notify_one();
        Ok(task.id)
    }

    /// Live cron jobs (queued, running or operator-paused), next fire first.
    pub fn list_crons(&self) -> Result<Vec<ka_agent_core::Task>, Error> {
        Ok(self.sh.store.cron_tasks()?)
    }

    /// The engine home (skills, compiled memory).
    pub fn engine_home(&self) -> std::path::PathBuf {
        self.sh.paths.engine_home()
    }

    /// The instance store (read/write by routers and the web layer).
    pub fn store(&self) -> &Arc<Store> {
        &self.sh.store
    }

    /// Live reminders and crons, next fire first (`/reminders`).
    pub fn scheduled_tasks(&self) -> Result<Vec<ka_agent_core::Task>, Error> {
        Ok(self.sh.store.scheduled_tasks()?)
    }

    /// Cancel any scheduled task (reminder or cron) by id prefix.
    pub fn cancel_scheduled(&self, id_prefix: &str) -> Result<CancelOutcome, Error> {
        if id_prefix.trim().is_empty() {
            return Ok(CancelOutcome::NotFound);
        }
        let matches: Vec<_> = self
            .sh
            .store
            .scheduled_tasks()?
            .into_iter()
            .filter(|t| ka_agent_core::ids::id_matches(t.id.as_str(), id_prefix.trim()))
            .collect();
        match matches.as_slice() {
            [] => Ok(CancelOutcome::NotFound),
            [one] => {
                self.sh.store.set_task_status(&one.id, "cancelled")?;
                Ok(CancelOutcome::Cancelled(one.title.clone()))
            }
            many => Ok(CancelOutcome::Ambiguous(
                many.iter()
                    .map(|t| ka_agent_core::ids::short_id(t.id.as_str()).to_string())
                    .collect(),
            )),
        }
    }

    /// Fire due tasks (reminders and crons); called from the scheduler
    /// timer. Each item is claimed (`queued -> running`) before it acts and
    /// settled after; a concurrent cancel/pause wins both races (claim and
    /// settle are conditional), and one failing item never stops the batch.
    pub async fn fire_due_tasks(&self) -> Result<Vec<String>, Error> {
        let now = ka_agent_core::now_ms();
        let due = self.sh.store.due_tasks(now)?;
        let mut fired = Vec::new();
        // SEED.md §7.2: a fire that sat overdue fires late (and says so)
        // within the grace hour; beyond it, one-shots are recorded `missed`
        // and recurring jobs skip the missed occupation and re-arm.
        const LATE_GRACE_MS: i64 = 3_600_000;
        const LATE_MARK_MS: i64 = 60_000;
        let now_zoned = jiff::Timestamp::now().to_zoned(self.cfg().time_zone());
        // Anchored recurrence (`at_local`): the next occurrence is the next
        // local wall-clock hit (weekday-optional), DST-correct — not a
        // fixed-interval hop from the previous fire.
        let next_for = |spec: &ka_agent_core::CronSpec, now: i64| -> Option<i64> {
            let Some(at) = spec.at_local.as_deref() else {
                return next_occurrence(
                    now,
                    spec.recurrence_ms.unwrap_or(0),
                    ka_agent_core::now_ms(),
                );
            };
            let (hh, mm) = ka_agent_core::when::parse_hhmm(at)?;
            let weekday = spec
                .weekday
                .and_then(|w| jiff::civil::Weekday::from_monday_zero_offset(w as i8).ok());
            Some(ka_agent_core::when::next_local(hh, mm, weekday, &now_zoned))
        };
        let late_by = |task: &ka_agent_core::Task| now - task.scheduled_at_ms.unwrap_or(now);
        let late_suffix = |late_by: i64| {
            if late_by > LATE_MARK_MS {
                format!(" (late by {})", ka_agent_core::fmt_interval(late_by))
            } else {
                String::new()
            }
        };
        for task in due {
            // Claim: the snapshot is stale the moment another surface
            // (cancel, pause, remove) touched the row. Skip what we lost.
            if !self.sh.store.claim_task(&task.id).unwrap_or(false) {
                continue;
            }
            match task.kind.as_str() {
                "reminder" => {
                    // A reminder is a notification, not a prompt: firing
                    // it must not spend a model turn. Work that needs the
                    // engine is a cron job with `mode = agent`, chosen
                    // explicitly by the operator.
                    let title = task.title.clone();
                    let this_late_by = late_by(&task);
                    if this_late_by > LATE_GRACE_MS {
                        self.sh.surface.on_notice(&format!(
                            "⏰ missed: {title} (overdue by {})",
                            ka_agent_core::fmt_interval(this_late_by)
                        ));
                        let _ = self.sh.store.settle_task(&task.id, "missed");
                        continue;
                    }
                    self.sh
                        .surface
                        .on_notice(&format!("⏰ {title}{}", late_suffix(this_late_by)));
                    let _ = self.sh.store.settle_task(&task.id, "succeeded");
                    fired.push(title);
                }
                "cron" => {
                    let Some(spec) = ka_agent_core::CronSpec::from_payload(
                        task.payload.as_deref().unwrap_or(""),
                    ) else {
                        let _ = self.sh.store.settle_task(&task.id, "failed");
                        continue;
                    };
                    let this_late_by = late_by(&task);
                    let recurring = spec.recurrence_ms.is_some_and(|rec| rec > 0);
                    if this_late_by > LATE_GRACE_MS {
                        if recurring {
                            // A recurring job outlives one missed occupation:
                            // skip it and re-arm strictly after now.
                            let note = match next_for(&spec, task.scheduled_at_ms.unwrap_or(now)) {
                                Some(next) => {
                                    let _ = self.sh.store.rearm_task(&task.id, next);
                                    format!(
                                        "cron '{}' skipped a missed occurrence (overdue by {}); re-armed",
                                        spec.text,
                                        ka_agent_core::fmt_interval(this_late_by)
                                    )
                                }
                                None => {
                                    let _ = self.sh.store.settle_task(&task.id, "failed");
                                    "cron re-arm overflow".to_string()
                                }
                            };
                            self.sh.surface.on_notice(&note);
                            continue;
                        }
                        self.sh.surface.on_notice(&format!(
                            "cron '{}' missed (overdue by {})",
                            spec.text,
                            ka_agent_core::fmt_interval(this_late_by)
                        ));
                        let _ = self.sh.store.settle_task(&task.id, "missed");
                        continue;
                    }
                    let ok = match &spec.mode {
                        ka_agent_core::CronMode::Agent => {
                            self.sh.surface.on_notice(&format!(
                                "cron: {}{}",
                                spec.text,
                                late_suffix(this_late_by)
                            ));
                            match self.handle_message(&spec.text, "cron").await {
                                Ok(()) => true,
                                Err(e) => {
                                    self.sh
                                        .surface
                                        .on_error("cron", &format!("{}: {e}", task.id.as_str()));
                                    false
                                }
                            }
                        }
                        ka_agent_core::CronMode::NoAgent(kind) => {
                            match self.run_no_agent(kind, &spec.text) {
                                Ok(out) => {
                                    self.sh.surface.on_notice(&out);
                                    true
                                }
                                Err(e) => {
                                    self.sh
                                        .surface
                                        .on_error("cron", &format!("{}: {e}", task.id.as_str()));
                                    false
                                }
                            }
                        }
                    };
                    if !ok {
                        // Failed jobs are not re-armed: the failure is
                        // visible on the surface and the row stays `failed`.
                        let _ = self.sh.store.settle_task(&task.id, "failed");
                        continue;
                    }
                    match spec.recurrence_ms {
                        Some(rec) if rec > 0 => {
                            match next_for(&spec, task.scheduled_at_ms.unwrap_or(now)) {
                                // Re-arm only if the row is still `running` (a
                                // cancel mid-flight wins).
                                Some(next) => {
                                    let _ = self.sh.store.rearm_task(&task.id, next);
                                }
                                // Overflowing schedule: unarmable, fail visibly.
                                None => {
                                    self.sh.surface.on_error(
                                        "cron",
                                        &format!("{}: recurrence overflow", task.id.as_str()),
                                    );
                                    let _ = self.sh.store.settle_task(&task.id, "failed");
                                    continue;
                                }
                            }
                        }
                        Some(_) => {
                            let _ = self.sh.store.settle_task(&task.id, "failed");
                            continue;
                        }
                        None => {
                            let _ = self.sh.store.settle_task(&task.id, "succeeded");
                        }
                    }
                    fired.push(spec.text);
                }
                other => {
                    self.sh
                        .surface
                        .on_error("cron", &format!("unknown scheduled kind {other:?}"));
                    let _ = self.sh.store.settle_task(&task.id, "failed");
                }
            }
        }
        Ok(fired)
    }

    /// The maintenance loop: one wake for the approval-expiry sweep and
    /// the scheduler. Sleeps until the next due task when one is queued
    /// (bounded by `every`), otherwise a plain `every` interval — an idle
    /// daemon ticks once per interval, not per concern. `scheduler_tick`
    /// interrupts the sleep so an in-process `/remind` or `/cron add`
    /// recomputes the deadline immediately; engine-side `cron_manage`
    /// writes the store from another process and waits at most `every`.
    pub async fn run_maintenance(&self, every: Duration) {
        loop {
            if let Err(e) = self.expire_approvals().await {
                self.sh
                    .surface
                    .on_error("internal", &format!("expiry sweep: {e}"));
            }
            if let Err(e) = self.fire_due_tasks().await {
                self.sh
                    .surface
                    .on_error("internal", &format!("scheduler: {e}"));
            }
            self.reap_idle_worker().await;
            let wait = match self.sh.store.next_due_ms() {
                Ok(Some(next)) => {
                    let now = ka_agent_core::now_ms();
                    if next <= now {
                        Duration::ZERO
                    } else {
                        Duration::from_millis((next - now) as u64 + 100)
                    }
                }
                _ => every,
            };
            // Floor guards against a spin when a due task cannot be
            // claimed; the cap bounds cross-process scheduling latency.
            let wait = wait.min(every).max(Duration::from_millis(250));
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.sh.scheduler_tick.notified() => {}
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
        self.submit_text_with(text, Vec::new()).await
    }

    /// Refuse a message that cannot possibly work, before any state moves:
    /// no model configured, or the model's provider key missing. The
    /// refusal is the operator's answer; the session returns to idle and
    /// nothing is submitted or recorded.
    fn preflight(&self) -> Result<(), String> {
        // Scripted speakers (e2e) legitimately run without model or keys.
        if self.sh.port.scenario_override.is_some() {
            return Ok(());
        }
        let cfg = self.cfg();
        if cfg.model.is_none() && cfg.ka.engine_layer.is_none() {
            return Err(
                "no model configured — run `ka-agent setup` (or /model provider/model)".into(),
            );
        }
        if let Some(model) = &cfg.model {
            let sel = ka_agent_port::catalog::validate_selector(model)?;
            let env = ka_agent_port::catalog::read_env_file(
                &ka_agent_port::catalog::engine_env_path(&self.sh.paths.engine_home()),
            );
            if let ka_agent_port::catalog::KeyState::Missing(name) =
                ka_agent_port::catalog::key_state(&sel, &env)
            {
                return Err(format!(
                    "{model} needs {name} — on your computer run: ka-agent key set {name}"
                ));
            }
        }
        Ok(())
    }

    /// Submit with per-turn skills (`Command::Prompt.skills`: the engine
    /// injects the full SKILL.md bodies for these names into this turn).
    async fn submit_text_with(&self, text: &str, skills: Vec<String>) -> Result<(), Error> {
        if let Err(msg) = self.preflight() {
            self.sh.surface.on_notice(&msg);
            let mut g = self.lock();
            if g.session.status == SessionStatus::Running {
                g.session.transition(SessionStatus::Idle).ok();
            }
            drop(g);
            self.persist_session_status(SessionStatus::Idle);
            return Ok(());
        }
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
        // Claim the turn for this live session *before* the submit handshake:
        // the engine can stream and finish the turn while we are still
        // awaiting `port.submit`, and a late claim loses `turn_finished`
        // (the record would never go terminal, and the session would never
        // return to idle).
        {
            let mut g = self.lock();
            if let Some(live) = g.live.as_mut() {
                live.current_turn = Some(turn.id.clone());
                live.live_text.clear();
            }
        }
        match port.submit(text.to_string(), Vec::new(), skills).await {
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
        // The engine may have finished the turn already (fast scripted
        // speaker): its record is then terminal and there is nothing left
        // for this handshake to record.
        if self
            .sh
            .store
            .get_turn(&turn.id)
            .ok()
            .flatten()
            .is_some_and(|t| t.status.is_terminal())
        {
            return Ok(());
        }
        turn.transition(TurnStatus::Running)
            .map_err(|t| illegal_turn(turn.status, t))?;
        self.sh.store.update_turn(&turn)?;
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
        let cfg = self.cfg();
        let wait = Backoff::new(
            cfg.worker_restart.backoff_start_ms,
            cfg.worker_restart.backoff_max_ms,
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
            let layer = cfg
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
            model: cfg.model.clone(),
            mode: Some(cfg.mode.clone()),
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
                deliberate_close: false,
                last_activity_ms: ka_agent_core::now_ms(),
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
        // Any event is activity: the idle reap's clock resets.
        {
            let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(live) = g.live.as_mut() {
                live.last_activity_ms = ka_agent_core::now_ms();
            }
        }
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
                    && let Err(e) = sh.store.set_strand(&snapshot.id, sid_ref, path_ref)
                {
                    sh.surface
                        .on_error("internal", &format!("strand persist: {e}"));
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
                        cfg_of(&sh).approval_ttl_secs,
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
                // An ask is only actionable while its turn is live. The engine
                // can raise one after it already reported `idle` (the scripted
                // speaker finishes its response before the permission gate, and
                // a real engine can lose the same race), and no surface can
                // settle an ask for a turn that is over. Deny it (fail closed)
                // so the engine is never left blocked on its gate.
                let (awaiting, live_port, status) = {
                    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let from = g.session.status;
                    let awaiting = g
                        .session
                        .transition(SessionStatus::AwaitingApproval)
                        .map_err(|t| illegal_session(from, t))
                        .is_ok();
                    if awaiting && let Some(live) = g.live.as_mut() {
                        live.pending_asks
                            .insert(approval.id.clone(), ask_id.clone());
                    }
                    let port = g.live.as_ref().map(|l| Arc::clone(&l.port));
                    let status = g.session.status;
                    drop(g);
                    (awaiting, port, status)
                };
                if awaiting {
                    rt_persist_status(&sh, status);
                    if let Some(card) = card {
                        sh.surface.on_approval(&card);
                    }
                    continue;
                }
                if let Some(port) = live_port {
                    let _ = port
                        .answer(ask_id.clone(), ApprovalDecision::Deny.ka_choice())
                        .await;
                }
                if let Err(e) = sh.store.decide_approval(
                    &approval.id,
                    ApprovalStatus::Denied,
                    ka_agent_core::now_ms(),
                ) {
                    sh.surface
                        .on_error("internal", &format!("ask deny persist: {e}"));
                }
                let mut rec = AuditRecord::new(
                    "runtime",
                    audit_actions::APPROVAL_DECIDE,
                    approval.id.as_str(),
                    Some("deny: the turn this ask belongs to already finished".into()),
                );
                let _ = sh.store.append_audit(&mut rec);
                sh.surface
                    .on_notice("ask auto-denied: its turn already finished (nothing to approve)");
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
                if let Some(tid) = turn_result
                    && let Some(mut t) = sh.store.get_turn(&tid).ok().flatten()
                {
                    t.stop = Some(stop.clone());
                    t.cost_usd = cost_usd;
                    t.tokens = tokens;
                    // The engine can finish a turn before the submit
                    // handshake recorded it as running: walk the record
                    // up so the finish always lands on a legal edge.
                    if t.status == TurnStatus::Submitted {
                        t.transition(TurnStatus::Running).ok();
                        sh.store.update_turn(&t).ok();
                    }
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
                    sh.surface.on_notice(
                        "daily budget reached — paused until tomorrow (override via CLI)",
                    );
                }
                if stop == "error" {
                    sh.surface
                        .on_notice("turn ended in error; see /status — nothing was re-submitted");
                }
                // A cycle can emit several `turn_finished` events (auto-poke,
                // verify): the first settles the status, the later ones only
                // refresh the cumulative usage.
                rt_maybe_idle(&sh);
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
                // `idle` is a delivery boundary, not the end of the turn. ka
                // reports it for the cycle — including while a submitted turn
                // is still open (engine start-up, a permission gate about to
                // open) — and acting on it as if the turn were over orphans
                // the turn: later messages would be submitted on top of it
                // instead of queuing, an ask would arrive for a session that
                // no longer looks busy, and draining the queue here would
                // hand ka a prompt it drops (`PortError::Busy`, gap G4). The
                // turn's own record decides.
                let turn_open = {
                    let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let status = g.session.status;
                    let current = g.live.as_ref().and_then(|l| l.current_turn.clone());
                    drop(g);
                    match current {
                        // A turn is in flight until its own record says it is over.
                        Some(tid) => sh
                            .store
                            .get_turn(&tid)
                            .ok()
                            .flatten()
                            .is_some_and(|t| !t.status.is_terminal()),
                        // No turn recorded yet, but an inbound message moved the
                        // session to `running`: its prompt is still being handed
                        // over, and the engine's start-up `idle` must not end it
                        // (that would make the next message overtake the prompt
                        // ka is about to accept).
                        None => status == SessionStatus::Running,
                    }
                };
                // A pending ask owns the boundary too: the engine is blocked
                // until its decision or TTL arrives.
                let ask_pending = {
                    let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.live.as_ref().is_some_and(|l| !l.pending_asks.is_empty())
                };
                if !ask_pending && !turn_open {
                    drain_queue(&sh).await;
                }
                rt_maybe_idle(&sh);
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
                    let remedy = cfg_of(&sh)
                        .model
                        .as_deref()
                        .and_then(|m| ka_agent_port::catalog::validate_selector(m).ok())
                        .and_then(|s| s.key_env);
                    let text = match remedy {
                        Some(env) => {
                            format!(
                                "the provider rejected the key — run `ka-agent key set {env}`, then /resume"
                            )
                        }
                        None => {
                            "auth failure — paused; fix the provider key and /resume".to_string()
                        }
                    };
                    sh.surface.on_notice(&text);
                }
            }
            PortEvent::EngineClosed => {
                let interrupted = {
                    let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.live.as_ref().and_then(|l| l.current_turn.clone())
                };
                if let Some(tid) = interrupted
                    && let Some(mut t) = sh.store.get_turn(&tid).ok().flatten()
                    && !t.status.is_terminal()
                {
                    t.transition(TurnStatus::Interrupted)
                        .map_err(|x| illegal_turn(t.status, x))
                        .ok();
                    sh.store.update_turn(&t).ok();
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

/// Return the session to `idle` once nothing is left in flight: the turn's
/// record is terminal (or no turn is recorded and no prompt is being handed
/// over), no ask is pending and the queue is empty. Called from both the
/// `turn_finished` and `idle` handlers — the turn record going terminal is
/// what proves the cycle is over, so the engine's `idle` event is only the
/// delivery boundary and its timing can never stall the status.
fn rt_maybe_idle(sh: &Arc<Shared>) {
    let (sid, status, ask_pending, current) = {
        let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
        (
            g.session.id.clone(),
            g.session.status,
            g.live.as_ref().is_some_and(|l| !l.pending_asks.is_empty()),
            g.live.as_ref().and_then(|l| l.current_turn.clone()),
        )
    };
    if status == SessionStatus::Idle || ask_pending {
        return;
    }
    // No turn recorded, but an inbound message set the session `running`: its
    // prompt is still being handed over, so the engine's `idle` is not the
    // end of it.
    let turn_open = match current {
        Some(tid) => sh
            .store
            .get_turn(&tid)
            .ok()
            .flatten()
            .is_some_and(|t| !t.status.is_terminal()),
        None => status == SessionStatus::Running,
    };
    let queued = sh.store.queue_len(&sid).unwrap_or(0);
    if turn_open || queued > 0 {
        return;
    }
    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
    let from = g.session.status;
    g.session
        .transition(SessionStatus::Idle)
        .map_err(|t| illegal_session(from, t))
        .ok();
    let status = g.session.status;
    drop(g);
    rt_persist_status(sh, status);
    sh.surface.on_session_status(status);
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
    let cfg = cfg_of(sh);
    let window_ms = (cfg.worker_restart.crash_window_secs as i64) * 1000;
    let deliberate = {
        let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.live.as_ref().is_some_and(|l| l.deliberate_close)
    };
    let (status, note) = {
        let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.live = None;
        if !deliberate {
            g.crashes.retain(|t| now - *t < window_ms);
            g.crashes.push(now);
            g.restart_attempt = g.restart_attempt.saturating_add(1);
        }
        let degraded = !deliberate && g.crashes.len() >= cfg.worker_restart.crash_max as usize;
        let from = g.session.status;
        let target = if degraded {
            SessionStatus::Degraded
        } else {
            SessionStatus::Idle
        };
        let note = if degraded {
            "engine closed repeatedly — session degraded; /new to recover".to_string()
        } else if deliberate {
            // `/new`, a `/stop` escalation, or the idle reap: intentional.
            "worker stopped; the next message restarts it".to_string()
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
    if deliberate {
        sh.surface.on_note(&note);
    } else {
        sh.surface.on_notice(&note);
    }
}

/// Seed the workspace with an `AGENTS.md` context file (M11). Written only
/// when absent — an operator's own file is never touched.
fn write_agents_md(cfg: &DaemonConfig) -> std::io::Result<()> {
    let ws = cfg.workspace_path();
    let path = ws.join("AGENTS.md");
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&ws)?;
    std::fs::write(&path, AGENTS_MD)
}

const AGENTS_MD: &str = r#"# Ka Agent

This workspace is served by ka-agent, a resident personal assistant running
on the Ka engine. One operator per instance; everything is auditable.

- Memory: durable facts live under `<data>/memory/` and are compiled into the
  engine's MEMORY.md. Propose new facts with the `memory_propose` tool for
  operator review; never store them yourself.
- Operator commands: /help lists them all (/status /new /stop /resume
  /model /remember /memory /history /remind /reminders /cron /calendar
  /approvals /skills /y /n).
- Scheduling: reminders and crons use the operator's local time. One-shot
  `/cron add in 10m <text>`; recurring `/cron add every 1h <text>`,
  `/cron add every day at 08:00 <text>` or
  `/cron add every mon at 09:00 <text>`. Reminders accept `in 2h30m`,
  `at 18:30`, `tomorrow 9am`, `fri 17:00` or `2026-11-03 10:00`. The
  `cron_manage` tool does the same from the engine ('at' takes RFC 3339
  with the operator's UTC offset).
- References: `@<file>`, `@<dir>/`, `@turn:<id-prefix>`, `@https://…` inline
  in a message inject that context into the turn.
- External effects (HTTP writes, GitHub) require operator grants and
  approvals; a daily budget applies.
- Health and audit: `ka-agent doctor`, `ka-agent history`; backups under
  `<data>/backups/`.
"#;

/// Next occurrence strictly after `now`, skipping missed occupations.
/// `None` on i64 overflow (an unarmable schedule, e.g. a hand-crafted
/// payload with an absurd interval).
fn next_occurrence(base_ms: i64, recurrence_ms: i64, now: i64) -> Option<i64> {
    let mut next = base_ms.checked_add(recurrence_ms)?;
    while next <= now {
        next = next.checked_add(recurrence_ms)?;
    }
    Some(next)
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
