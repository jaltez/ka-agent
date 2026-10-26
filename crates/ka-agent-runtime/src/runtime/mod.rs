//! The runtime core: single-active-session orchestration over the port.
//!
//! Responsibilities (SEED.md §3.5–§3.8): single-flight prompt submission with
//! a durable queue, approval lifecycle with deny-on-expiry, budget
//! enforcement, lazy worker (re)start with backoff and a crash-loop breaker,
//! and delivery of exactly one final answer per prompt cycle at `idle`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

use crate::event::{AgentEvent, Envelope};
use crate::paths::Paths;
use crate::surface::Surface;

mod approvals;
mod config;
mod events;
mod identity;
mod intake;
mod memory;
mod scheduler;
mod worker;

use config::publish_timezone;

pub use config::{ReloadReport, memory_preamble};
pub use events::split_final_text;
pub use identity::{LEGACY_AGENTS_MD_SHA256, compile_identity, legacy_agents_md_status};
pub use scheduler::CancelOutcome;
pub use worker::Backoff;

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

struct Live {
    port: Arc<PortSession>,
    /// ask_id of the engine ask behind each pending approval.
    pending_asks: HashMap<ApprovalId, String>,
    live_text: String,
    current_turn: Option<TurnId>,
    /// Who started the running turn (`console`, `telegram`, `cron`, …):
    /// `TurnStarted` carries it and later steps gate on it.
    current_turn_source: Option<String>,
    /// When the running turn started (its `TurnFinished` reports the
    /// elapsed time).
    turn_started_at: Option<Instant>,
    /// When each in-flight tool call started (its `ToolFinished` reports
    /// the elapsed time).
    tool_started: HashMap<String, Instant>,
    /// The last output excerpt per tool call id (≤ 300 chars): the error
    /// line of a failed call.
    tool_excerpt: HashMap<String, String>,
    /// The tools this worker's engine discovered (for `/tools`).
    tools: Vec<String>,
    /// The strand's latest display title (status, not broadcast).
    title: Option<String>,
    /// We killed this worker on purpose (`/new`, the `/stop` escalation): its
    /// exit must not be booked as a crash.
    deliberate_close: bool,
    /// Last time anything happened on this session (event, submit, ask) —
    /// the idle reap's clock. `now_ms` while a turn is in flight.
    last_activity_ms: i64,
    /// The latest tool call (name, argument summary): the engine's
    /// permission question for MCP tools carries no command, so the card
    /// names the call instead of reading "run ``?".
    last_call: Option<(String, String)>,
}

struct Inner {
    session: Session,
    /// Calls the operator allowed "for this session" (question + detail,
    /// exact): asked again, they are answered without a card. Cleared by
    /// `/new`.
    session_allow: std::collections::HashSet<String>,
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
    /// The envelope sequence (starts at 1): every emitted envelope takes
    /// one, so any stream can name the last event it saw.
    seq: AtomicU64,
    inner: Mutex<Inner>,
    /// No-agent cron handlers, registered by kind (e.g. "calendar").
    /// Registered by the app binary; the runtime only dispatches (M11).
    no_agent: Mutex<HashMap<String, NoAgentFn>>,
    /// Preflight cache: the validated selector per model string (a
    /// config reload swaps the key; validation then runs once more).
    selector_cache: Mutex<Option<(String, ka_agent_port::catalog::SelectorInfo)>>,
    /// The engine `.env` map, keyed by the file's mtime — re-read only
    /// when the file changed (key set / rotate touches it).
    env_cache: Mutex<
        Option<(
            std::time::SystemTime,
            std::collections::BTreeMap<String, String>,
        )>,
    >,
    /// Wakes the scheduler when a task is scheduled sooner than its sleep.
    scheduler_tick: tokio::sync::Notify,
    /// The config file's mtime as last seen by the runtime (any reload
    /// updates it): the maintenance loop's hot-reload watcher compares
    /// against it so IPC-triggered reloads are not announced twice.
    config_mtime: Mutex<Option<std::time::SystemTime>>,
    /// The persona file's mtime as last seen (identity watcher).
    persona_mtime: Mutex<Option<std::time::SystemTime>>,
    /// When this runtime woke up (ms since epoch), for the header.
    started_ms: i64,
}

impl Shared {
    /// Sequence and fan out one event to the surface.
    pub(super) fn emit(&self, ev: AgentEvent) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.surface.emit(&Envelope::new(seq, ev));
    }
}

fn cfg_of(sh: &Shared) -> Arc<DaemonConfig> {
    Arc::clone(&sh.cfg.read().unwrap_or_else(|e| e.into_inner()))
}

/// Whether the engine can resolve the configured model's provider key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelKeyState {
    Present,
    Missing(String),
    Keyless,
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
        // The sequence exists before `Shared` does: the recovery notices
        // below are envelopes too (a subscribed early receiver sees them).
        let seq = AtomicU64::new(1);
        let emit = |ev: AgentEvent| {
            surface.emit(&Envelope::new(seq.fetch_add(1, Ordering::Relaxed), ev));
        };
        let interrupted = store.reconcile_running_turns(ka_agent_core::now_ms())?;
        if !interrupted.is_empty() {
            emit(AgentEvent::Notice {
                text: format!(
                    "recovered after restart: {} interrupted turn(s) marked; nothing was re-submitted",
                    interrupted.len()
                ),
            });
        }
        // A crash mid-fire leaves scheduled tasks `running`; without this
        // they sit invisible to the scheduler forever while `/cron list`
        // still shows them. Marked `interrupted` (= paused, resumable).
        let stuck_tasks = store.reconcile_running_tasks(ka_agent_core::now_ms())?;
        if !stuck_tasks.is_empty() {
            emit(AgentEvent::Notice {
                text: format!(
                    "recovered after restart: {} scheduled task(s) interrupted mid-fire marked paused",
                    stuck_tasks.len()
                ),
            });
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
                emit(AgentEvent::Notice {
                    text: format!(
                        "recovered session status {from:?} → idle (its turn died with the previous process)"
                    ),
                });
            }
        }
        let session = session;
        publish_timezone(&store, &cfg);
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
        let runtime = Self {
            sh: Arc::new(Shared {
                cfg: std::sync::RwLock::new(Arc::new(cfg)),
                config_path: Mutex::new(None),
                paths,
                store,
                memory,
                port,
                surface,
                seq,
                no_agent: Mutex::new(HashMap::new()),
                selector_cache: Mutex::new(None),
                env_cache: Mutex::new(None),
                scheduler_tick: tokio::sync::Notify::new(),
                config_mtime: Mutex::new(None),
                persona_mtime: Mutex::new(None),
                started_ms: ka_agent_core::now_ms(),
                inner: Mutex::new(Inner {
                    session,
                    session_allow: std::collections::HashSet::new(),
                    live: None,
                    budget,
                    crashes: Vec::new(),
                    restart_attempt: 0,
                }),
            }),
        };
        // The assistant's identity (persona + house rules) compiled into
        // the engine's IDENTITY.md; re-read per turn, so persona edits
        // need no restart. A read-only engine home must not fail startup.
        if let Err(e) = runtime.refresh_identity() {
            eprintln!("runtime: IDENTITY.md not written: {e}");
        }
        // Remove our own leftover seeded AGENTS.md (identity moved to
        // IDENTITY.md); an operator's modified copy is never touched
        // (doctor reports leftovers).
        if let Some(path) = identity::legacy_agents_md_status(&runtime.workspace())
            && let Err(e) = std::fs::remove_file(&path)
        {
            eprintln!("runtime: legacy AGENTS.md not removed: {e}");
        }
        Ok(runtime)
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
            persona: self.persona_name(),
            awake_since_ms: self.sh.started_ms,
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
            capabilities: crate::status::Capabilities::from_config(&cfg),
            memory: self.memory_report().ok().map(|r| crate::status::MemoryUse {
                used_bytes: r.total_bytes,
                budget_bytes: r.budget_bytes,
                truncated: r.truncated,
                to_review: self
                    .sh
                    .store
                    .pending_candidates_full()
                    .map(|c| c.len())
                    .unwrap_or(0),
            }),
        })
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
            // "Allow for this session" ends with the session.
            g.session_allow.clear();
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
        self.sh.emit(AgentEvent::Session {
            status: format!("{:?}", SessionStatus::Idle),
        });
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

    fn persist_session_status(&self, status: SessionStatus) {
        let id = self.session_id();
        if let Err(e) = self.sh.store.update_session_status(&id, status) {
            self.sh.emit(AgentEvent::Error {
                class: "internal".into(),
                message: format!("session status persist: {e}"),
            });
        }
    }

    /// Sequence and fan out one event to every surface.
    pub fn emit(&self, ev: AgentEvent) {
        self.sh.emit(ev);
    }

    // ---- memory & history (SEED.md §4) --------------------------------

    /// The engine home (skills, compiled memory).
    pub fn engine_home(&self) -> std::path::PathBuf {
        self.sh.paths.engine_home()
    }

    /// The instance store (read/write by routers and the web layer).
    pub fn store(&self) -> &Arc<Store> {
        &self.sh.store
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
    fn spawn_delay_zero_on_cold_start_then_backs_off() {
        let b = Backoff::new(1000, 60_000);
        assert_eq!(b.spawn_delay(0), Duration::ZERO, "cold start is immediate");
        assert_eq!(b.spawn_delay(1), Duration::from_millis(1000));
        assert_eq!(b.spawn_delay(2), Duration::from_millis(2000));
        assert_eq!(b.spawn_delay(7), Duration::from_millis(60_000));
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
