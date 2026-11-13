// Worker lifecycle per user: lazy spawn with backoff, the global worker cap
// with LRU idle eviction, idle reap, deliberate stop, engine-closed recovery.
// Split from runtime.rs (moves only).
use super::events::{drive, rt_persist_status};
use super::*;
use ka_agent_core::Principal;

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

    /// Delay before spawn attempt `n`: a cold start (attempt 0) spawns
    /// immediately; restart attempts back off exponentially.
    pub fn spawn_delay(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            Duration::ZERO
        } else {
            self.delay(attempt - 1)
        }
    }
}

impl Runtime {
    /// The live-worker ceiling (`runtime.max_workers`, clamped by config
    /// validation to 1..=64).
    pub fn max_workers(&self) -> usize {
        self.cfg().runtime.max_workers.max(1)
    }

    /// Whether any user's turn is live: the self-update guard refuses to
    /// restart the binary mid-answer, for every account (ADR-0004).
    pub fn any_turn_active(&self) -> bool {
        self.sh
            .users
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|st| {
                matches!(
                    st.lock().session.status,
                    SessionStatus::Running | SessionStatus::AwaitingApproval | SessionStatus::Stopping
                )
            })
    }

    /// How many users currently have a live worker (`/status`, doctor and
    /// the concurrency tests read it).
    pub fn live_worker_count(&self) -> usize {
        self.sh
            .users
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|st| st.lock().live.is_some())
            .count()
    }

    /// Lazily (re)start `who`'s worker with backoff and the crash-loop
    /// breaker. Falls back to a fresh strand when the recorded strand file no
    /// longer exists (SEED.md §3.5).
    pub(super) async fn ensure_worker(
        &self,
        who: &Principal,
        source: &str,
    ) -> Result<(), Error> {
        let state = self.ensure_user(who)?;
        // One spawn handshake at a time: the cross-user cap check and the
        // `live` claim must not interleave, or two simultaneous submits both
        // observe a free slot and the ceiling is exceeded.
        let _permit = self.sh.spawn_lock.lock().await;
        if state.lock().live.is_some() {
            return Ok(());
        }
        // The pool cap: an idle worker of another user makes room first; a
        // fully busy pool makes this message wait for the next free slot.
        self.make_room_for_worker(&who.username);
        if self.live_worker_count() >= self.max_workers() {
            return Err(Error::Refused(format!(
                "all {} engine workers are busy — your message stays queued for the next free slot",
                self.max_workers()
            )));
        }
        let cfg = self.cfg();
        let attempt = state.lock().restart_attempt;
        let wait = Backoff::new(
            cfg.worker_restart.backoff_start_ms,
            cfg.worker_restart.backoff_max_ms,
        )
        .spawn_delay(attempt);
        if !wait.is_zero() {
            // A crash-loop backoff can freeze the next message for tens of
            // seconds; say so instead of looking hung.
            if wait >= Duration::from_secs(2) {
                // The asking surface hears the delay (a Slack/Discord user
                // must not wait in silence).
                self.sh.emit_from(
                    AgentEvent::Note {
                        text: format!("respawning worker in {}s (backoff)", wait.as_secs()),
                    },
                    Some(source),
                );
            }
            tokio::time::sleep(wait).await;
        }
        let (req, session, was_private) = self.prepare_open(who, &state, &cfg)?;
        let _ = was_private;
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
            let mut g = state.lock();
            g.restart_attempt = 0;
            g.last_active_ms = ka_agent_core::now_ms();
            g.live = Some(Live {
                port: Arc::clone(&port),
                pending_asks: HashMap::new(),
                live_text: String::new(),
                current_turn: None,
                current_turn_source: None,
                turn_started_at: None,
                tool_started: HashMap::new(),
                tool_excerpt: HashMap::new(),
                tools: Vec::new(),
                title: None,
                deliberate_close: false,
                last_activity_ms: ka_agent_core::now_ms(),
                watchdog_notified: false,
                last_call: None,
            });
        }
        tokio::spawn(drive(
            Arc::clone(&self.sh),
            who.clone(),
            state.clone(),
            port,
            rx,
        ));
        Ok(())
    }

    /// Everything a worker spawn needs, computed under short locks: the
    /// isolation environment, the permission mode, the engine layer and the
    /// strand to attach to. `Err` is operator-facing (a refused spec).
    fn prepare_open(
        &self,
        who: &Principal,
        state: &UserHandle,
        cfg: &DaemonConfig,
    ) -> Result<(OpenRequest, Session, bool), Error> {
        let role = who.role.clone();
        let is_admin = role == ka_agent_core::ADMIN_ROLE;
        // Non-admin users own a disjoint tree: HOME, KA_DATA_DIR,
        // XDG_STATE_HOME and the cache all resolve under
        // `<data>/users/<username>`, so strands, skills and compiled memory
        // can never mix with anyone else's.
        let spec = (!is_admin).then(|| self.effective_role(&role));
        let (session, mode, env, user_layer, was_private) = {
            let g = state.lock();
            let mode = if is_admin {
                cfg.mode.clone()
            } else {
                g.session_mode.clone().unwrap_or_else(|| {
                    spec.as_ref()
                        .and_then(|s| s.mode_canonical())
                        .unwrap_or_else(|| "readonly".to_string())
                })
            };
            // A private conversation's engine state (strands, waypoints, and
            // the spill files oversized tool outputs park under
            // XDG_STATE_HOME) lives in a per-session scratch directory the
            // runtime destroys when the session ends.
            let scratch = g.session.private.then(|| {
                let dir = private_engine_dir(
                    self.sh.paths.root(),
                    ka_agent_core::ids::short_id(g.session.id.as_str()),
                );
                let _ = std::fs::create_dir_all(&dir);
                dir
            });
            let data_dir = scratch.clone().unwrap_or_else(|| {
                if is_admin {
                    self.sh.paths.ka_data()
                } else {
                    self.sh.paths.user_ka_data(&who.username)
                }
            });
            let state_dir = scratch.unwrap_or_else(|| {
                if is_admin {
                    self.sh.paths.engine_state()
                } else {
                    self.sh.paths.user_engine_state(&who.username)
                }
            });
            let home = if is_admin {
                self.sh.paths.engine_home()
            } else {
                self.sh.paths.user_engine_home(&who.username)
            };
            let cache_dir = if is_admin {
                self.sh.paths.engine_cache()
            } else {
                self.sh.paths.user_engine_cache(&who.username)
            };
            // Grants: the profile's for admin; a user sees ONLY their role's
            // [[role.grants.path]].
            let (install, full, deny, allow) = match &spec {
                None => (
                    cfg.grants.install || cfg.grants.full,
                    cfg.grants.full,
                    cfg.grants
                        .path
                        .iter()
                        .filter(|g| g.access == ka_agent_core::config::Access::Deny)
                        .map(|g| g.dir.clone())
                        .collect(),
                    cfg.grants
                        .path
                        .iter()
                        .filter(|g| g.access == ka_agent_core::config::Access::Allow)
                        .map(|g| g.dir.clone())
                        .collect(),
                ),
                Some(spec) => (
                    false,
                    false,
                    spec.grants
                        .path
                        .iter()
                        .filter(|g| g.access == ka_agent_core::config::Access::Deny)
                        .map(|g| g.dir.clone())
                        .collect(),
                    spec.grants
                        .path
                        .iter()
                        .filter(|g| g.access == ka_agent_core::config::Access::Allow)
                        .map(|g| g.dir.clone())
                        .collect(),
                ),
            };
            let env = WorkerEnv {
                home,
                data_dir,
                state_dir,
                cache_dir,
                tool_context: Some(who.username.clone()),
                agent_data_root: self.sh.paths.root().to_path_buf(),
                guard_write: self.guard_write_paths(&who.username, &role),
                guard_read: self.guard_read_paths(&who.username, &role),
                // Grant decisions ride the same pre_tool_use hook contract as
                // the protected lists (KA_AGENT_GUARD_GRANTS). `hard` marks
                // that no operator gate remains for bash (allow mode or
                // full): then the whole data root is protected.
                guard_grants: ka_agent_port::GuardGrants {
                    install,
                    full,
                    deny,
                    allow,
                    hard: full || mode == "allow",
                },
            };
            let user_layer = cfg
                .engine_layer_path()
                .and_then(|p| std::fs::read_to_string(p).ok());
            (
                g.session.clone(),
                mode,
                env,
                user_layer,
                g.session.private,
            )
        };
        let _ = user_root_marker(&self.sh.paths, &who.username);
        // Provider keys live in the profile engine home; a user's worker
        // reads them through a symlink so key rotation stays in sync.
        provision_user_env(&self.sh.paths, &who.username);
        // Ka Agent's own engine features (search, fallback, MCP servers, the
        // profile's and the role's feature toggles) join the operator's
        // layer; `secret:` values resolve here, in memory, from the secret
        // store.
        let secrets_dir = self.sh.paths.root().join("secrets");
        let role_spec =
            (role != ka_agent_core::ADMIN_ROLE).then(|| self.effective_role(&role));
        let engine_layer_toml = ka_agent_port::engine_layer::compose_for_role(
            cfg,
            role_spec.as_ref(),
            user_layer.as_deref(),
            &|name| ka_agent_core::secret::resolve(name, &secrets_dir).map_err(|e| e.to_string()),
        )
        .map_err(Error::Refused)?;
        let selector = match (&session.strand_path, session.strand_id.as_ref()) {
            (Some(p), Some(_)) if p.exists() => StrandSelector::Path(p.clone()),
            _ => StrandSelector::New,
        };
        let (binary, worker_args) = self.worker_command(&who.username);
        let req = OpenRequest {
            workspace: session.workspace.clone(),
            strand: selector,
            model: cfg.model.clone(),
            mode: Some(mode),
            engine_layer_toml,
            env,
            binary,
            worker_args,
            scenario: None,
            auto_allow: self.auto_allow(),
        };
        Ok((req, session, was_private))
    }

    /// The worker command for a user: the daemon binary's hidden `__worker`,
    /// or that binary under the engine bubblewrap profile when
    /// `runtime.sandbox_engine` is on (ADR-0003: measured, off by default).
    fn worker_command(&self, username: &str) -> (std::path::PathBuf, Vec<String>) {
        let exe = if self.sh.port.binary.as_os_str().is_empty() {
            std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("ka-agent"))
        } else {
            self.sh.port.binary.clone()
        };
        let args = vec!["__worker".to_string()];
        if !self.cfg().runtime.sandbox_engine {
            return (exe, args);
        }
        let Some(bwrap) = which("bwrap") else {
            eprintln!("runtime: sandbox_engine is on but bwrap is not in PATH; running unsandboxed");
            return (exe, args);
        };
        let user_root = self.sh.paths.user_root(username);
        let mut full: Vec<String> = Vec::new();
        let ro_bind = |full: &mut Vec<String>, src: &str| {
            if std::path::Path::new(src).exists() {
                full.push("--ro-bind".into());
                full.push(src.into());
                full.push(src.into());
            }
        };
        for dir in ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"] {
            ro_bind(&mut full, dir);
        }
        // The binary itself: production installs live under /usr; a build
        // tree needs its own read-only bind.
        let mut exe_in_sandbox = exe.clone();
        if let (Some(parent), Some(name)) = (exe.parent(), exe.file_name())
            && !parent.starts_with("/usr")
            && !parent.starts_with("/bin")
        {
            full.push("--ro-bind".into());
            full.push(parent.to_string_lossy().into_owned());
            full.push("/opt/ka".into());
            exe_in_sandbox = std::path::Path::new("/opt/ka").join(name);
        }
        for arg in ["--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"] {
            full.push(arg.to_string());
        }
        full.push("--bind".into());
        full.push(user_root.to_string_lossy().into_owned());
        full.push(user_root.to_string_lossy().into_owned());
        // Read-only: the provider keys every worker authenticates with —
        // provision_user_env symlinks the user's engine-home .env at this
        // shared target, and the sandbox must expose the target itself.
        let env_path = self.sh.paths.engine_home().join(".config/ka/.env");
        if env_path.exists() {
            full.push("--ro-bind".into());
            let p = env_path.to_string_lossy().into_owned();
            full.push(p.clone());
            full.push(p);
        }
        // A session workspace outside user_root (an explicit role workspace
        // or a private session's temp dir) must still be writable.
        if let Ok(canon_root) = std::fs::canonicalize(&user_root) {
            for ws in self.session_workspaces_outside(&username, &canon_root) {
                full.push("--bind".into());
                let p = ws.to_string_lossy().into_owned();
                full.push(p.clone());
                full.push(p);
            }
        }
        for arg in ["--unshare-pid", "--die-with-parent"] {
            full.push(arg.to_string());
        }
        full.push("--".into());
        full.push(exe_in_sandbox.to_string_lossy().into_owned());
        full.extend(args);
        (bwrap, full)
    }

    /// Session workspaces the sandboxed worker needs that live outside the
    /// user's own tree: an explicit role-spec workspace and any private
    /// engine temp dir their sessions use.
    fn session_workspaces_outside(
        &self,
        username: &str,
        canon_user_root: &std::path::Path,
    ) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let Ok(sessions) = self.sh.store.sessions_for_user(username) else {
            return out;
        };
        for ws in sessions.into_iter().map(|s| s.workspace) {
            let Ok(canon) = std::fs::canonicalize(&ws) else {
                continue;
            };
            if !canon.starts_with(canon_user_root) && !out.contains(&canon) {
                out.push(canon);
            }
        }
        out
    }

    /// Make room under the worker cap by stopping the least recently active
    /// *settled* worker of another user. Returns whether one was stopped.
    pub(super) fn make_room_for_worker(&self, requester: &str) -> bool {
        if self.live_worker_count() < self.max_workers() {
            return false;
        }
        self.evict_lru_idle_worker(requester)
    }

    /// Evict one idle worker (LRU): deliberate close, the session and strand
    /// persist, the owner's next message reopens it. Never evicts a user
    /// mid-turn.
    pub(super) fn evict_lru_idle_worker(&self, requester: &str) -> bool {
        for (username, st) in self.users_by_activity() {
            if username == requester {
                continue;
            }
            let port = {
                let mut g = st.lock();
                if g.session.status != SessionStatus::Idle {
                    continue;
                }
                let Some(live) = g.live.as_ref() else {
                    continue;
                };
                let settled = live.pending_asks.is_empty()
                    && self.sh.store.queue_len(&g.session.id).unwrap_or(0) == 0
                    && live.current_turn.as_ref().is_none_or(|t| {
                        self.sh
                            .store
                            .get_turn(t)
                            .ok()
                            .flatten()
                            .is_none_or(|t| t.status.is_terminal())
                    });
                if !settled {
                    continue;
                }
                let port = Arc::clone(&live.port);
                if let Some(live) = g.live.as_mut() {
                    live.deliberate_close = true;
                }
                port
            };
            // The kill is asynchronous; run it on the current task without
            // blocking the caller's submit (the port's kill is bounded).
            tokio::spawn(async move { port.kill().await });
            return true;
        }
        false
    }

    /// Stop a worker that has had nothing to do for
    /// `worker_idle_timeout_secs`, for every user. Minimal-footprint rule: an
    /// idle resident must not pay for a resident engine.
    pub(super) async fn reap_idle_workers(&self) {
        let timeout_secs = self.cfg().worker_idle_timeout_secs;
        if timeout_secs == 0 {
            return;
        }
        for (_, st) in self.users_by_activity() {
            self.stop_settled_worker_state(&st, (timeout_secs as i64) * 1000)
                .await;
        }
    }

    /// Stop one user's worker now if settled (config reload, key change, or
    /// the session picker): the next message starts a fresh worker.
    pub async fn stop_idle_worker(&self) -> bool {
        let mut stopped = false;
        for (_, st) in self.users_by_activity() {
            stopped |= self.stop_settled_worker_state(&st, 0).await;
        }
        stopped
    }

    /// Stop `who`'s worker now if settled.
    pub async fn stop_idle_worker_for(&self, who: &Principal) -> bool {
        match self.user_handle(&who.username) {
            Some(st) => self.stop_settled_worker_state(&st, 0).await,
            None => false,
        }
    }

    /// Deliberately kill a settled worker that has been inactive for at
    /// least `min_idle_ms`.
    async fn stop_settled_worker_state(&self, st: &UserHandle, min_idle_ms: i64) -> bool {
        let now = ka_agent_core::now_ms();
        let port = {
            let mut g = st.lock();
            let (settled, idle_long_enough, port) = {
                let Some(live) = g.live.as_ref() else {
                    return false;
                };
                let idle_long_enough = now - live.last_activity_ms >= min_idle_ms;
                let maybe = g.session.status == SessionStatus::Idle && idle_long_enough;
                if !maybe {
                    (false, false, Arc::clone(&live.port))
                } else {
                    let turn_open = live.current_turn.as_ref().is_some_and(|t| {
                        self.sh
                            .store
                            .get_turn(t)
                            .ok()
                            .flatten()
                            .is_some_and(|t| !t.status.is_terminal())
                    });
                    let settled = !turn_open
                        && live.pending_asks.is_empty()
                        && self.sh.store.queue_len(&g.session.id).unwrap_or(0) == 0;
                    (settled, idle_long_enough, Arc::clone(&live.port))
                }
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
}

/// Resolve an executable in `PATH` (no external crate).
fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Ensure the user's partition exists before a worker writes into it.
fn user_root_marker(paths: &crate::paths::Paths, username: &str) -> std::io::Result<()> {
    paths.ensure_user_dirs(username)
}

/// Give a user's worker the profile's provider keys: symlink the profile
/// engine-home `.env` (key rotation follows the link), copying where symlinks
/// are unavailable.
fn provision_user_env(paths: &crate::paths::Paths, username: &str) {
    if username == ka_agent_core::OPERATOR_USER {
        return;
    }
    let src = paths.engine_home().join(".config").join("ka").join(".env");
    let dst = paths
        .user_engine_home(username)
        .join(".config")
        .join("ka")
        .join(".env");
    if !src.exists() {
        return;
    }
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if dst.exists() {
        #[cfg(not(unix))]
        {
            let src_ms = std::fs::metadata(&src).and_then(|m| m.modified()).ok();
            let dst_ms = std::fs::metadata(&dst).and_then(|m| m.modified()).ok();
            if src_ms > dst_ms {
                let _ = std::fs::copy(&src, &dst);
            }
        }
        return;
    }
    #[cfg(unix)]
    {
        if std::os::unix::fs::symlink(&src, &dst).is_err() {
            let _ = std::fs::copy(&src, &dst);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::copy(&src, &dst);
    }
}

/// One user's worker died or was killed: book it against that user only.
pub(super) async fn rt_on_engine_closed(
    sh: &Arc<Shared>,
    st: &UserHandle,
    port: Arc<PortSession>,
) {
    port.kill().await;
    let now = ka_agent_core::now_ms();
    let cfg = cfg_of(sh);
    let window_ms = (cfg.worker_restart.crash_window_secs as i64) * 1000;
    let deliberate = {
        let g = st.lock();
        g.live.as_ref().is_some_and(|l| l.deliberate_close)
    };
    // The turn's own source: crash notices go where the turn came from
    // (a Slack/Discord user must hear their turn died).
    let turn_source = {
        let g = st.lock();
        g.live.as_ref().and_then(|l| l.current_turn_source.clone())
    };
    let (status, note, lost_turn) = {
        let mut g = st.lock();
        let lost_turn = if deliberate {
            None
        } else {
            g.live.as_ref().and_then(|l| l.current_turn.clone())
        };
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
        (target, note, lost_turn)
    };
    // A crash mid-turn must say *what* died, not just that something did.
    let note = if !deliberate && status != SessionStatus::Degraded {
        if let Some(id) = lost_turn {
            let first = sh
                .store
                .get_turn(&id)
                .ok()
                .flatten()
                .map(|t| t.inbound_text)
                .unwrap_or_default();
            let first_line = first.lines().next().unwrap_or("").trim();
            if first_line.is_empty() {
                note
            } else {
                let head: String = first_line.chars().take(80).collect();
                format!("engine closed mid-answer; \"{head}\" was interrupted — /retry re-sends it")
            }
        } else {
            note
        }
    } else {
        note
    };
    for id in sh.store.expire_pending(now).unwrap_or_default() {
        sh.emit(AgentEvent::ApprovalSettled {
            id: id.as_str().to_string(),
            status: format!("{:?}", ka_agent_core::ApprovalStatus::Expired),
        });
    }
    // Only the asking surface hears about it first; the admin's channel is
    // not spammed with per-user crashes.
    let source = turn_source.clone();
    rt_persist_status(sh, st);
    sh.emit(AgentEvent::Session {
        status: format!("{status:?}"),
    });
    if deliberate {
        sh.emit_from(AgentEvent::Note { text: note.clone() }, source.as_deref());
    } else {
        sh.emit_from(AgentEvent::Notice { text: note }, source.as_deref());
    }
    // Another user's message may be waiting for a worker slot: this one
    // just freed capacity.
    Runtime { sh: Arc::clone(sh) }.spawn_drain();
}
