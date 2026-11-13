//! The runtime core: per-user conversational state over one bounded pool of
//! engine workers (ADR-0003).
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
mod policy;
mod roles;
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
    /// The "still working" notice for the current silent stretch already
    /// went out (the watchdog notifies once per stretch).
    watchdog_notified: bool,
    /// The latest tool call (name, argument summary): the engine's
    /// permission question for MCP tools carries no command, so the card
    /// names the call instead of reading "run ``?".
    last_call: Option<(String, String)>,
}

/// One user's conversational state: what `Inner` carried when the daemon
/// served a single subject, now one instance per user (ADR-0003).
pub(super) struct UserState {
    pub session: Session,
    /// The role the user acts with (from `users.role`).
    pub role: String,
    /// A `/mode` choice inside the role's ceiling for this session (a
    /// non-admin role never persists it).
    session_mode: Option<String>,
    /// A `/model` choice inside the user's policy for this session (never
    /// persisted outside the config for the operator).
    session_model: Option<String>,
    /// The shared agent this conversation runs as (`/agent <name>`), if any.
    agent: Option<String>,
    /// Calls the operator allowed "for this session" (question + detail,
    /// exact): asked again, they are answered without a card. Cleared by
    /// `/new`.
    session_allow: std::collections::HashSet<String>,
    live: Option<Live>,
    budget: BudgetState,
    /// Timestamps (ms) of recent worker crashes, for the crash-loop breaker.
    crashes: Vec<i64>,
    restart_attempt: u32,
    /// This user's reasoning effort (`/effort`; cleared by `/new`).
    effort: Option<String>,
    /// Last time this user did anything: the LRU order the worker cap's
    /// idle eviction uses.
    last_active_ms: i64,
}

/// A user's state handle: cheap to clone, locked only for short critical
/// sections (never across an await), so users' turns run in parallel.
#[derive(Clone)]
pub struct UserHandle(Arc<Mutex<UserState>>);

impl UserHandle {
    pub(super) fn new(state: UserState) -> Self {
        Self(Arc::new(Mutex::new(state)))
    }

    /// The state; a poisoned lock (a panic elsewhere) still yields the data.
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, UserState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The host's knowledge-base sync entry: called on each maintenance tick, it
/// decides which sources are due and spawns their connector (the HTTP and
/// git work lives in the channels crate, which the runtime cannot depend
/// on).
pub type KbSyncFn = Arc<dyn Fn() + Send + Sync>;

/// A no-agent cron handler: input is the spec text, output the digest text.
pub type NoAgentFn = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

/// The host-provided self-update entry (`/update`): `true` = check only,
/// `false` = install. Returns the operator-facing reply, or an error
/// message that already names the remedy. Registered by the app binary
/// (the release machinery lives there), exactly like the no-agent
/// handlers.
pub type UpdateFn = Arc<dyn Fn(bool) -> Result<String, String> + Send + Sync>;

struct Shared {
    /// The live configuration; swapped whole by [`Runtime::reload_config`].
    cfg: std::sync::RwLock<Arc<DaemonConfig>>,
    /// The file the configuration came from (for `/model` and reloads).
    config_path: Mutex<Option<std::path::PathBuf>>,
    paths: Paths,
    store: Arc<Store>,
    /// One memory store per user, opened lazily at the user's root
    /// (the operator keeps `<data>/memory`).
    memories: Mutex<HashMap<String, ka_agent_memory::MemoryStore>>,
    port: WorkerPort,
    surface: Arc<dyn Surface>,
    /// The envelope sequence (starts at 1): every emitted envelope takes
    /// one, so any stream can name the last event it saw.
    seq: AtomicU64,
    /// Every user's conversational state, keyed by username (ADR-0003).
    users: Mutex<HashMap<String, UserHandle>>,
    /// Serializes the cross-user worker-cap check with the spawn handshake
    /// (two users submitting in the same instant must not both spawn past
    /// `runtime.max_workers`).
    spawn_lock: tokio::sync::Mutex<()>,
    /// The host's knowledge-base sync hook (`[kb_sources]` rows).
    kb_sync: Mutex<Option<KbSyncFn>>,
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
    /// Serializes config writers with the hot-reload watcher: a reload
    /// that loaded a stale file must not undo a live setter, and a setter
    /// must not race a reload's apply.
    config_lock: tokio::sync::Mutex<()>,
    /// The persona file's mtime as last seen (identity watcher).
    persona_mtime: Mutex<Option<std::time::SystemTime>>,
    /// When this runtime woke up (ms since epoch), for the header.
    started_ms: i64,
    /// Exact tool names that never ask (the app computes this from the
    /// tool registry and sets it at host start).
    auto_allow: Mutex<Vec<String>>,
    /// Per-channel poll health, set by the channel tasks: a dead phone
    /// link must show in `status`, not only in the journal (F8).
    channel_health: Mutex<HashMap<String, crate::status::ChannelHealth>>,
    /// The model the engine last reported as answering (a fallback hop),
    /// when it differs from the configured selector (F3).
    live_model: Mutex<Option<String>>,
    /// The newest release seen by the daemon's periodic check (F4); the
    /// status line and one Notice surface it.
    update_available: Mutex<Option<String>>,
    /// The host's `/update` implementation (`None` where self-update is
    /// unavailable — tests, embedders).
    updater: Mutex<Option<UpdateFn>>,
    /// `/update` install in flight (single-flight guard: the install
    /// child and the delayed self-restart must never run twice at once).
    updating: std::sync::atomic::AtomicBool,
    /// The models.dev snapshot for the interactive `/model` listing,
    /// refreshed by the host's periodic task (cache-first, 24 h TTL).
    model_registry: Mutex<Option<ka_agent_port::registry::Registry>>,
}

impl Shared {
    /// Sequence and fan out one event to the surface.
    pub(super) fn emit(&self, ev: AgentEvent) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.surface.emit(&Envelope::new(seq, ev));
    }

    /// Sequence and fan out one event stamped with the source it belongs
    /// to (per-identity delivery).
    pub(super) fn emit_from(&self, ev: AgentEvent, source: Option<&str>) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.surface.emit(&Envelope::from(seq, ev, source));
    }
}

fn cfg_of(sh: &Shared) -> Arc<DaemonConfig> {
    Arc::clone(&sh.cfg.read().unwrap_or_else(|e| e.into_inner()))
}

/// Whether an error of this class is worth a manual retry (↻): transient
/// provider-side conditions only. The engine's own report always wins
/// ([`AgentEvent::Error::retryable`] passes through); this maps the
/// runtime-emitted classes.
pub(crate) fn retryable_class(class: &str) -> bool {
    matches!(class, "rate_limit" | "network" | "overflow")
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
        // Pre-users installs kept each role's tree under `roles/<name>`:
        // move them into the per-user partitions once (ADR-0004 bootstrap).
        let moved = paths.migrate_role_trees()?;
        // Every configured role owns a disjoint tree; create it up front so
        // a role's first message never races directory creation.
        paths.ensure_role_dirs_for(&cfg.roles)?;
        // The sequence exists before `Shared` does: the recovery notices
        // below are envelopes too (a subscribed early receiver sees them).
        let seq = AtomicU64::new(1);
        let emit = |ev: AgentEvent| {
            surface.emit(&Envelope::new(seq.fetch_add(1, Ordering::Relaxed), ev));
        };
        for (name, target) in &moved {
            emit(AgentEvent::Notice {
                text: format!(
                    "moved the pre-users tree for {name} to {}",
                    target.display()
                ),
            });
        }
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
        // A crash between an external write and its result leaves the intent
        // `executing`: book it as `unknown` so re-proposing the same action
        // asks the admin to confirm instead of writing twice (ADR-0004).
        let unknown_intents = store.reconcile_executing_intents()?;
        if !unknown_intents.is_empty() {
            emit(AgentEvent::Notice {
                text: format!(
                    "recovered after restart: {} external-write intent(s) had an unknown outcome — re-proposing the same action will ask you to confirm",
                    unknown_intents.len()
                ),
            });
        }
        let stuck_tasks = store.reconcile_running_tasks(ka_agent_core::now_ms())?;
        if !stuck_tasks.is_empty() {
            emit(AgentEvent::Notice {
                text: format!(
                    "recovered after restart: {} scheduled task(s) interrupted mid-fire marked paused",
                    stuck_tasks.len()
                ),
            });
        }
        // Boot adopts the admin conversation: the daemon's own surfaces
        // (terminal, web, CLI) are the admin role; a role's session is
        // adopted on the first message from one of its identities.
        let mut session = store
            .latest_session_for_user(ka_agent_core::OPERATOR_USER)?
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

        // Private conversations that ended before this boot (superseded,
        // crashed, daemon stopped) leave nothing behind: their queued
        // text is purged and their strands and ephemeral attachments
        // destroyed. The resumed session keeps its context while the
        // conversation itself stays live.
        let ended_private: Vec<_> = store
            .private_sessions_purge_queue()?
            .into_iter()
            .filter(|s| s.id != session.id)
            .collect();
        destroy_private_remnants(paths.root(), &ended_private);
        sweep_orphan_private_engine_dirs(paths.root(), &session);
        // A daemon that resumes a live private conversation must say so:
        // surfaces that attached before the runtime existed (the TUI
        // subscribes through the early event rx) would otherwise start
        // with a stale, normal-looking status line.
        if session.private {
            emit(AgentEvent::Settings {
                model: None,
                mode: None,
                effort: None,
                private: Some(true),
            });
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
        let memory =
            ka_agent_memory::MemoryStore::open(&paths.user_memory(ka_agent_core::OPERATOR_USER))?;
        // Compile stable memory into each engine's user-tier MEMORY.md; ka
        // re-reads it per prompt (validation §5), so no worker restart is
        // needed after updates. Every role gets its own root compiled into
        // its own engine home.
        memory.install_compiled(
            &paths.engine_home().join(".config/ka"),
            ka_agent_memory::DEFAULT_BUDGET_BYTES,
            &memory_preamble(&cfg),
        )?;
        // Legacy `roles/<r>` trees were moved to `users/<r>` above; compile
        // MEMORY.md for every existing user instead, so invited users' first
        // workers already speak the operator language.
        for row in store.list_users().unwrap_or_default() {
            if row.username == ka_agent_core::OPERATOR_USER {
                continue;
            }
            paths.ensure_user_dirs(&row.username)?;
            if let Ok(user_memory) =
                ka_agent_memory::MemoryStore::open(&paths.user_memory(&row.username))
            {
                let _ = user_memory.install_compiled(
                    &paths.user_engine_home(&row.username).join(".config/ka"),
                    ka_agent_memory::DEFAULT_BUDGET_BYTES,
                    &memory_preamble(&cfg),
                );
            }
        }
        // The operator's own state; every other user is created on their
        // first message (`ensure_user`).
        let operator_state = UserState {
            session,
            role: ka_agent_core::ADMIN_ROLE.to_string(),
            session_mode: None,
            session_model: None,
            agent: None,
            session_allow: std::collections::HashSet::new(),
            live: None,
            budget,
            crashes: Vec::new(),
            restart_attempt: 0,
            effort: None,
            last_active_ms: ka_agent_core::now_ms(),
        };
        let mut users = HashMap::new();
        users.insert(
            ka_agent_core::OPERATOR_USER.to_string(),
            UserHandle::new(operator_state),
        );
        let mut memories = HashMap::new();
        memories.insert(ka_agent_core::OPERATOR_USER.to_string(), memory);
        let runtime = Self {
            sh: Arc::new(Shared {
                cfg: std::sync::RwLock::new(Arc::new(cfg)),
                config_path: Mutex::new(None),
                paths,
                store,
                memories: Mutex::new(memories),
                port,
                surface,
                seq,
                kb_sync: Mutex::new(None),
                no_agent: Mutex::new(HashMap::new()),
                users: Mutex::new(users),
                spawn_lock: tokio::sync::Mutex::new(()),
                selector_cache: Mutex::new(None),
                env_cache: Mutex::new(None),
                scheduler_tick: tokio::sync::Notify::new(),
                config_mtime: Mutex::new(None),
                config_lock: tokio::sync::Mutex::new(()),
                persona_mtime: Mutex::new(None),
                started_ms: ka_agent_core::now_ms(),
                auto_allow: Mutex::new(Vec::new()),
                channel_health: Mutex::new(HashMap::new()),
                live_model: Mutex::new(None),
                update_available: Mutex::new(None),
                updater: Mutex::new(None),
                updating: std::sync::atomic::AtomicBool::new(false),
                model_registry: Mutex::new(None),
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
        if let Some(path) = identity::legacy_agents_md_status(
            &runtime.workspace_for(&ka_agent_core::Principal::operator()),
        )
            && let Err(e) = std::fs::remove_file(&path)
        {
            eprintln!("runtime: legacy AGENTS.md not removed: {e}");
        }
        Ok(runtime)
    }

    // ---- introspection -------------------------------------------------

    /// One user's current session id (`None` before their first message).
    pub fn session_id_for(&self, who: &ka_agent_core::Principal) -> Option<SessionId> {
        self.user_handle(&who.username)
            .map(|st| st.lock().session.id.clone())
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
        // The file as it exists now is the config this runtime was built
        // from — nothing reloaded. Seeding the watcher's mtime here keeps
        // the first maintenance tick from announcing "config reloaded"
        // on every boot (a PC that starts periodically would notify the
        // phone each time for no change at all).
        self.remember_config_mtime();
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

    /// The user's workspace: where `@` file references resolve and where the
    /// engine's files live. It follows that user's live session, falling
    /// back to their partition before any session exists.
    pub fn workspace_for(&self, who: &ka_agent_core::Principal) -> std::path::PathBuf {
        if let Some(st) = self.user_handle(&who.username) {
            let session = st.lock().session.workspace.clone();
            if !session.as_os_str().is_empty() {
                return session;
            }
        }
        self.workspace_for_user(&who.username, &who.role)
    }

    /// Now, in the operator's zone (configured, else the system zone).
    pub fn now_zoned(&self) -> jiff::Zoned {
        jiff::Timestamp::now().to_zoned(self.cfg().time_zone())
    }

    /// One user's session status (`Idle` for a user with no state yet).
    pub fn session_status_for(&self, who: &ka_agent_core::Principal) -> SessionStatus {
        self.user_handle(&who.username)
            .map(|st| st.lock().session.status)
            .unwrap_or(SessionStatus::Idle)
    }

    /// Everything `/status` shows, for one principal, in one struct.
    pub fn status_report_for(
        &self,
        who: &ka_agent_core::Principal,
    ) -> Result<crate::status::StatusReport, Error> {
        let cfg = self.cfg();
        let st = self.ensure_user(who)?;
        let (sid, status, strand, worker_running, budget, crashes, private, role) = {
            let g = st.lock();
            (
                g.session.id.clone(),
                g.session.status,
                g.session.strand_id.clone(),
                g.live.is_some(),
                g.budget.clone(),
                g.crashes.len(),
                g.session.private,
                g.role.clone(),
            )
        };
        let (limit_usd, limit_tokens) = self.budget_caps_for(&who.username, &role);
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
        let channels = {
            let g = self
                .sh
                .channel_health
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut rows: Vec<_> = g.values().cloned().collect();
            rows.sort_by(|a, b| a.channel.cmp(&b.channel));
            rows
        };
        Ok(crate::status::StatusReport {
            session_id: sid.to_string(),
            persona: self.persona_name(),
            awake_since_ms: self.sh.started_ms,
            status: format!("{status:?}"),
            private,
            strand_id: strand,
            model: cfg.model.clone(),
            mode: self.effective_mode_for(&who.username),
            worker_running,
            idle_timeout_secs: cfg.worker_idle_timeout_secs,
            spent_usd: budget.spent_usd,
            spent_tokens: budget.spent_tokens,
            limit_usd,
            limit_tokens,
            queue,
            pending_approvals,
            crashes,
            next_task,
            timezone: tz.iana_name().unwrap_or("UTC").to_string(),
            capabilities: crate::status::Capabilities::from_config(&cfg),
            memory: self.memory_report_for(who).ok().map(|r| crate::status::MemoryUse {
                used_bytes: r.total_bytes,
                budget_bytes: r.budget_bytes,
                truncated: r.truncated,
                to_review: self.pending_candidates_for(who).map(|c| c.len()).unwrap_or(0),
            }),
            live_model: self
                .sh
                .live_model
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            update_available: self
                .sh
                .update_available
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            channels,
        })
    }

    /// Record a channel poll outcome (F8): success clears the error.
    pub fn note_channel_ok(&self, channel: &str) {
        let mut g = self
            .sh
            .channel_health
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = g.entry(channel.to_string()).or_default();
        entry.channel = channel.to_string();
        entry.ok_since_ms = ka_agent_core::now_ms();
        entry.last_error = None;
    }

    /// Record a channel poll failure; `ok_since_ms` keeps the last
    /// successful poll (0 when the channel never worked).
    pub fn note_channel_err(&self, channel: &str, err: &str) {
        let mut g = self
            .sh
            .channel_health
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = g.entry(channel.to_string()).or_default();
        entry.channel = channel.to_string();
        entry.last_error = Some(err.chars().take(160).collect());
    }

    /// Record the release a periodic check found (F4): `Some(version)`
    /// when newer than this binary, `None` when current. A new sighting
    /// emits exactly one Notice.
    pub fn set_update_available(&self, version: Option<String>) {
        let mut g = self
            .sh
            .update_available
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if g.as_deref() == version.as_deref() {
            return;
        }
        let announce = version.clone();
        *g = version;
        drop(g);
        if let Some(v) = announce {
            self.sh.emit(AgentEvent::Notice {
                text: format!("update available: {v} — /update installs it"),
            });
        }
    }

    /// Register the host's knowledge-base sync hook.
    pub fn set_kb_sync(&self, f: KbSyncFn) {
        *self.sh.kb_sync.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    /// One knowledge-sync tick: the hook decides what is due.
    pub fn run_kb_sync(&self) {
        let hook = self
            .sh
            .kb_sync
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Register the host's `/update` implementation (the app binary; the
    /// release machinery lives there, not in the runtime).
    pub fn set_updater(&self, f: UpdateFn) {
        *self.sh.updater.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    /// The registered `/update` implementation, if this host can
    /// self-update.
    pub fn updater(&self) -> Option<UpdateFn> {
        self.sh
            .updater
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Claim the single `/update` install slot: `false` when one is
    /// already in flight.
    pub fn try_begin_update(&self) -> bool {
        !self
            .sh
            .updating
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// Release the `/update` install slot.
    pub fn end_update(&self) {
        self.sh
            .updating
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Publish the models.dev snapshot the interactive `/model` listing
    /// reads (the host's periodic task refreshes it; `None` clears).
    pub fn set_model_registry(&self, reg: Option<ka_agent_port::registry::Registry>) {
        *self
            .sh
            .model_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = reg;
    }

    /// The published models.dev snapshot, when there is one.
    pub fn model_registry(&self) -> Option<ka_agent_port::registry::Registry> {
        self.sh
            .model_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The grouped, current-first listing `/model` with no argument
    /// shows on every surface (registry models merged over the catalog,
    /// recommended model first per provider).
    pub fn models_listing(&self) -> ka_agent_port::registry::ModelsListing {
        let reg = self.model_registry();
        let current = self.cfg().model.clone();
        ka_agent_port::registry::listing(reg.as_ref(), current.as_deref())
    }

    // ---- operator operations -------------------------------------------

    /// `/new`: drop the live worker and start a fresh session/strand.
    /// `private` starts an incognito conversation instead: turn text is
    /// never recorded, memory/skill proposals are refused, and the
    /// strand plus ephemeral attachments are destroyed when it ends.
    pub async fn new_session(
        &self,
        who: &ka_agent_core::Principal,
        private: bool,
    ) -> Result<SessionId, Error> {
        // `/new` kills the worker on purpose: its exit is not a crash, and a
        // fresh session starts with a clean crash history (five `/new` in ten
        // minutes used to degrade the brand-new session).
        // One critical section: mark the worker deliberately closed, take
        // the port, and swap the session — nothing can submit onto or
        // enqueue against the ended session once the guard releases.
        let st = self.ensure_user(who)?;
        let (superseded, live_port, session) = {
            let mut g = st.lock();
            if let Some(live) = g.live.as_mut() {
                live.deliberate_close = true;
            }
            g.crashes.clear();
            // "Allow for this session", the session mode and effort end
            // with it.
            g.session_allow.clear();
            g.session_mode = None;
            g.effort = None;
            let superseded = g.session.private.then(|| g.session.id.clone());
            let live_port = g.live.take().map(|l| l.port);
            let mut s = Session::new_for_user(
                self.workspace_for_user(&who.username, &who.role),
                &who.username,
                &who.role,
            );
            s.private = private;
            g.session = s;
            g.restart_attempt = 0;
            (superseded, live_port, g.session.clone())
        };
        // A private conversation ends here: destroy its queued text,
        // approval cards, strand and ephemeral attachments.
        if superseded.is_some() {
            self.purge_private_remnants();
        }
        if let Some(port) = live_port {
            port.kill().await;
        }
        self.sh.store.insert_session(&session)?;
        // A message racing this `/new` can still have enqueued onto the
        // ended session during the kill await; nothing can target it
        // anymore, so the idempotent sweep now catches every straggler.
        if superseded.is_some() {
            self.purge_private_remnants();
        }
        self.audit_for(
            who,
            audit_actions::SESSION_NEW,
            session.id.as_str(),
            private.then(|| "private".to_string()),
        )?;
        self.sh.emit(AgentEvent::Settings {
            model: None,
            mode: None,
            effort: None,
            private: Some(private),
        });
        self.sh.emit(AgentEvent::Session {
            status: format!("{:?}", SessionStatus::Idle),
        });
        Ok(session.id)
    }

    /// Whether this user's live session is a private (incognito)
    /// conversation.
    pub fn session_private_for(&self, who: &ka_agent_core::Principal) -> bool {
        self.user_handle(&who.username)
            .map(|st| st.lock().session.private)
            .unwrap_or(false)
    }

    /// The ephemeral-storage token for a private session (`Some(short
    /// id)`), naming its `.private-<token>` attachment directory; `None`
    /// for a normal session.
    pub fn private_session_token_for(&self, who: &ka_agent_core::Principal) -> Option<String> {
        let st = self.user_handle(&who.username)?;
        // Both under one guard: a `/new` between two locks could pair the
        // old session's privacy with the new session's id.
        let g = st.lock();
        g.session
            .private
            .then(|| ka_agent_core::ids::short_id(g.session.id.as_str()).to_string())
    }

    /// One idempotent private-remnant sweep: purge the store rows
    /// (queued text, approval cards) and destroy the on-disk remnants
    /// (scratch engine homes, strand files, ephemeral media) of every
    /// private session on record. Safe to re-run.
    fn purge_private_remnants(&self) {
        if let Ok(remnants) = self.sh.store.private_sessions_purge_queue() {
            destroy_private_remnants(self.sh.paths.root(), &remnants);
        }
    }

    /// `/resume`: lift a pause (budget override or a fixed auth key) back to
    /// idle. Refuses when the budget verdict is still exceeded — the
    /// operator asked explicitly, so say why instead of silently ignoring.
    pub async fn resume_for(&self, who: &ka_agent_core::Principal) -> Result<bool, Error> {
        let st = self.ensure_user(who)?;
        let mut g = st.lock();
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
        let id = g.session.id.clone();
        drop(g);
        self.persist_session_status(&who.username, &id, status);
        self.audit_for(who, audit_actions::SESSION_RESUME, id.as_str(), None)?;
        Ok(true)
    }

    pub(super) fn persist_session_status(
        &self,
        username: &str,
        id: &SessionId,
        status: SessionStatus,
    ) {
        let _ = username;
        if let Err(e) = self.sh.store.update_session_status(id, status) {
            self.sh.emit(AgentEvent::Error {
                class: "internal".into(),
                retryable: retryable_class("internal"),
                message: format!("session status persist: {e}"),
            });
        }
    }

    /// The source of the turn currently running (`console`, `cron:<id>`,
    /// `briefing`, …); `None` when idle. Read by the tool handlers' loop
    /// guards instead of a store meta row.
    pub fn current_turn_source_for(&self, who: &ka_agent_core::Principal) -> Option<String> {
        self.user_handle(&who.username)?
            .lock()
            .live
            .as_ref()
            .and_then(|l| l.current_turn_source.clone())
    }

    /// Ka-agent's own files the engine may never write (the `__guard`
    /// list): the live config, store, secrets, memory, engine homes, every
    /// other profile root and every other role's root. A role's own root
    /// (and workspace) stays writable; admin blocks the whole `roles/`
    /// tree.
    fn guard_write_paths(&self, username: &str, role: &str) -> Vec<std::path::PathBuf> {
        let root = self.sh.paths.root();
        let base = ka_agent_core::layout::base_dir();
        let mut out = vec![
            root.join("config.toml"),
            root.join("state.db"),
            root.join("state.db-wal"),
            root.join("state.db-shm"),
            root.join("admin.token"),
            root.join("secrets"),
            root.join("memory"),
            root.join("engine-home"),
            root.join("engine-state"),
            root.join("ka"),
            root.join("backups"),
            root.join("logs"),
            base.join("config.toml"),
            base.join("active_profile"),
            base.join("skills"),
        ];
        if role == ka_agent_core::ADMIN_ROLE {
            out.push(root.join("roles"));
            out.push(self.sh.paths.users_root());
            // A role's workspace is not the operator's: admin may not
            // write into it either (the admin workspace stays free).
            out.extend(self.cfg().roles.iter().map(|r| r.workspace_path(root)));
        } else {
            // Every other user's root, the whole roles/ tree and the
            // operator's profile tree are off limits.
            let own = self.sh.paths.user_root(username);
            if let Ok(entries) = std::fs::read_dir(self.sh.paths.users_root()) {
                for e in entries.flatten() {
                    if e.path() != own {
                        out.push(e.path());
                    }
                }
            }
            out.push(root.join("roles"));
            out.push(self.cfg().workspace_path());
        }
        if let Ok(entries) = std::fs::read_dir(base.join("profiles")) {
            for e in entries.flatten() {
                // The acting instance's own root holds its workspace.
                if e.path() != root {
                    out.push(e.path());
                }
            }
        }
        out
    }

    /// Files the engine may never read: the secret store, the admin token,
    /// the engine `.env`, every other profile's secrets and (for a role)
    /// the profile's own memory, notes, strands and store.
    fn guard_read_paths(&self, username: &str, role: &str) -> Vec<std::path::PathBuf> {
        let root = self.sh.paths.root();
        let base = ka_agent_core::layout::base_dir();
        let mut out = vec![
            root.join("secrets"),
            root.join("admin.token"),
            self.sh.paths.engine_home().join(".config/ka/.env"),
        ];
        // Every other user's root, and the pre-users role trees.
        let own = self.sh.paths.user_root(username);
        if let Ok(entries) = std::fs::read_dir(self.sh.paths.users_root()) {
            for e in entries.flatten() {
                if e.path() != own {
                    out.push(e.path());
                }
            }
        }
        out.push(root.join("roles"));
        if role != ka_agent_core::ADMIN_ROLE {
            // A subject never reads the operator's conversation, memory or
            // notes: the profile's own tree is off limits.
            out.extend([
                root.join("memory"),
                root.join("notes"),
                root.join("ka"),
                root.join("engine-home"),
                root.join("engine-state"),
                root.join("state.db"),
                root.join("config.toml"),
            ]);
        }
        if let Ok(entries) = std::fs::read_dir(base.join("profiles")) {
            for e in entries.flatten() {
                let secrets = e.path().join("secrets");
                if e.path() != root {
                    out.push(secrets);
                }
            }
        }
        out
    }

    /// Register the approval profile: exact tool names the engine never
    /// asks about (reads, proposals, local writes, granted search).
    pub fn set_auto_allow(&self, names: Vec<String>) {
        *self.sh.auto_allow.lock().unwrap_or_else(|e| e.into_inner()) = names;
    }

    fn auto_allow(&self) -> Vec<String> {
        self.sh
            .auto_allow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Sequence and fan out one event to every surface.
    pub fn emit(&self, ev: AgentEvent) {
        self.sh.emit(ev);
    }

    // ---- memory & history (SEED.md §4) --------------------------------

    /// The instance's directory layout (export/purge and the web panel).
    pub fn paths(&self) -> Paths {
        self.sh.paths.clone()
    }

    /// The data root this runtime was opened on.
    pub fn data_root(&self) -> std::path::PathBuf {
        self.sh.paths.root().to_path_buf()
    }

    /// The engine home (skills, compiled memory).
    pub fn engine_home(&self) -> std::path::PathBuf {
        self.sh.paths.engine_home()
    }

    /// One role's synthetic engine HOME (skills, compiled memory); admin
    /// is the profile's own.
    pub fn role_engine_home(&self, role: &str) -> std::path::PathBuf {
        self.sh.paths.role_engine_home(role)
    }

    /// The instance store (read/write by routers and the web layer).
    pub fn store(&self) -> &Arc<Store> {
        &self.sh.store
    }

    fn live_port_for(&self, who: &ka_agent_core::Principal) -> Option<Arc<PortSession>> {
        self.user_handle(&who.username)?
            .lock()
            .live
            .as_ref()
            .map(|l| Arc::clone(&l.port))
    }

    /// The live worker's bounded stderr tail (diagnostics: doctor, tests).
    pub fn worker_stderr_tail(&self) -> Vec<String> {
        self.live_port_for(&ka_agent_core::Principal::operator())
            .map(|p| p.stderr_tail())
            .unwrap_or_default()
    }

    pub(super) fn audit_for(
        &self,
        who: &ka_agent_core::Principal,
        action: &str,
        subject: &str,
        detail: Option<String>,
    ) -> Result<(), Error> {
        let mut rec = AuditRecord::new("operator", action, subject, detail)
            .with_role(&who.role)
            .with_user(&who.username);
        self.sh.store.append_audit(&mut rec)?;
        Ok(())
    }
    // ---- submission ------------------------------------------------------
}

/// The per-session scratch engine home of a private (incognito)
/// conversation: the worker's `KA_DATA_DIR`, so its strands and waypoints
/// never mix into the shared engine home.
fn private_engine_dir(root: &std::path::Path, session_short: &str) -> std::path::PathBuf {
    root.join("tmp")
        .join(format!("private-engine-{session_short}"))
}

/// Destroy the durable remnants of ended private (incognito)
/// conversations: each session's scratch engine directory (strands,
/// waypoints), its recorded strand file and every ephemeral `.private-*`
/// attachment directory under its workspace. Idempotent and best-effort —
/// anything already gone is success, and the next sweep retries what
/// failed.
fn destroy_private_remnants(root: &std::path::Path, sessions: &[Session]) {
    for s in sessions {
        let short = ka_agent_core::ids::short_id(s.id.as_str());
        let _ = std::fs::remove_dir_all(private_engine_dir(root, short));
        if let Some(strand) = &s.strand_path {
            let _ = std::fs::remove_file(strand);
        }
        let attachments = s.workspace.join("attachments");
        let Ok(entries) = std::fs::read_dir(&attachments) else {
            continue;
        };
        // Exact-name match: sessions share one workspace, so a prefix
        // glob would also destroy a *live* private session's media.
        let private_dir = format!(".private-{short}");
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy() == private_dir {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

/// Remove scratch engine directories no live private session owns — a
/// crash can orphan them (the session row is the sweeper's usual source,
/// but a wiped store or a copied root should not keep conversation
/// strands either). The adopted session's directory survives while that
/// conversation stays live.
fn sweep_orphan_private_engine_dirs(root: &std::path::Path, keep: &Session) {
    let keep_short = ka_agent_core::ids::short_id(keep.id.as_str());
    let parent = root.join("tmp");
    let Ok(entries) = std::fs::read_dir(&parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("private-engine-") && name != format!("private-engine-{keep_short}") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
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
