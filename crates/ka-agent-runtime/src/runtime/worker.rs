// Worker lifecycle: lazy spawn with backoff, idle reap, deliberate stop,
// engine-closed recovery. Split from runtime.rs (moves only).
use super::events::{drive, rt_persist_status};
use super::*;

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
    /// Lazily (re)start the worker with backoff and the crash-loop breaker.
    /// Falls back to a fresh strand when the recorded strand file no longer
    /// exists (SEED.md §3.5).
    pub(super) async fn ensure_worker(&self) -> Result<(), Error> {
        if self.lock().live.is_some() {
            return Ok(());
        }
        let cfg = self.cfg();
        let wait = Backoff::new(
            cfg.worker_restart.backoff_start_ms,
            cfg.worker_restart.backoff_max_ms,
        )
        .spawn_delay(self.lock().restart_attempt);
        if !wait.is_zero() {
            // A crash-loop backoff can freeze the next message for tens of
            // seconds; say so instead of looking hung.
            if wait >= Duration::from_secs(2) {
                self.sh.emit(AgentEvent::Note {
                    text: format!("respawning worker in {}s (backoff)", wait.as_secs()),
                });
            }
            tokio::time::sleep(wait).await;
        }
        let admin = ka_agent_core::ADMIN_ROLE;
        let (session, role, mode, env, engine_layer_toml) = {
            let g = self.lock();
            let role = g.role.clone();
            let is_admin = role == admin;
            // Non-admin roles own a disjoint tree: HOME, KA_DATA_DIR,
            // XDG_STATE_HOME and the cache all resolve under
            // `<data>/roles/<name>`, so strands, skills and compiled
            // memory can never mix with the profile's.
            let spec = (!is_admin).then(|| self.effective_role(&role));
            let mode = if is_admin {
                cfg.mode.clone()
            } else {
                g.session_mode.clone().unwrap_or_else(|| {
                    spec.as_ref()
                        .and_then(|s| s.mode_canonical())
                        .unwrap_or_else(|| "readonly".to_string())
                })
            };
            // A private conversation's engine state (strands, waypoints,
            // and the spill files oversized tool outputs park under
            // XDG_STATE_HOME) lives in a per-session scratch directory
            // the runtime destroys when the session ends — nothing
            // durable of the conversation stays in shared engine dirs.
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
                    self.sh.paths.role_ka_data(&role)
                }
            });
            let state_dir = scratch.unwrap_or_else(|| {
                if is_admin {
                    self.sh.paths.engine_state()
                } else {
                    self.sh.paths.role_engine_state(&role)
                }
            });
            let home = if is_admin {
                self.sh.paths.engine_home()
            } else {
                self.sh.paths.role_engine_home(&role)
            };
            let cache_dir = if is_admin {
                self.sh.paths.engine_cache()
            } else {
                self.sh.paths.role_engine_cache(&role)
            };
            // Grants: the profile's for admin; a role sees ONLY its own
            // [[role.grants.path]] (the profile's operator grants do not
            // apply to roles — they are not the operator).
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
                agent_data_root: self.sh.paths.root().to_path_buf(),
                guard_write: self.guard_write_paths(&role),
                guard_read: self.guard_read_paths(&role),
                // Grant decisions ride the same pre_tool_use hook
                // contract as the protected lists
                // (KA_AGENT_GUARD_GRANTS). `hard` marks that no
                // operator gate remains for bash (allow mode or
                // full): then the whole data root is protected, not
                // just ka-agent's enumerated own files.
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
            (g.session.clone(), role, mode, env, user_layer)
        };
        // Provider keys live in the profile engine home; a role's worker
        // reads them through a symlink so key rotation stays in sync.
        provision_role_env(&self.sh.paths, &role);
        // Ka Agent's own engine features (search, fallback, MCP servers,
        // the profile's and the role's feature toggles) join the
        // operator's layer; `secret:` values resolve here, in memory,
        // from the secret store.
        let secrets_dir = self.sh.paths.root().join("secrets");
        let role_spec = (role != admin).then(|| self.effective_role(&role));
        let engine_layer_toml = ka_agent_port::engine_layer::compose_for_role(
            &cfg,
            role_spec.as_ref(),
            engine_layer_toml.as_deref(),
            &|name| ka_agent_core::secret::resolve(name, &secrets_dir).map_err(|e| e.to_string()),
        )
        .map_err(Error::Refused)?;
        let selector = match (&session.strand_path, session.strand_id.as_ref()) {
            (Some(p), Some(_)) if p.exists() => StrandSelector::Path(p.clone()),
            _ => StrandSelector::New,
        };
        let req = OpenRequest {
            workspace: session.workspace.clone(),
            strand: selector,
            model: cfg.model.clone(),
            mode: Some(mode),
            engine_layer_toml,
            env,
            binary: self.sh.port.binary.clone(),
            worker_args: vec!["__worker".to_string()],
            scenario: None,
            auto_allow: self.auto_allow(),
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
        tokio::spawn(drive(Arc::clone(&self.sh), port, rx));
        Ok(())
    }

    /// Stop a worker that has had nothing to do for
    /// `worker_idle_timeout_secs`. Minimal-footprint rule: an idle resident
    /// must not pay for a resident engine. The kill is `deliberate_close`
    /// — not a crash, no backoff, no degradation — and the next message
    /// restarts the worker, resuming the strand by path. Skipped unless
    /// the session is fully settled: idle status, no open turn, no pending
    /// ask, nothing queued.
    pub(super) async fn reap_idle_worker(&self) {
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
                // Cheap in-memory gates first: the SQL reads below run on
                // every scheduler tick, and an idle daemon with its worker
                // already reaped (or mid-turn) has no business querying.
                let idle_long_enough = now - live.last_activity_ms >= min_idle_ms;
                let maybe = g.session.status == SessionStatus::Idle && idle_long_enough;
                if !maybe {
                    (false, false, live.port.clone())
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
                    (settled, idle_long_enough, live.port.clone())
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

/// Give a role's worker the profile's provider keys: symlink the profile
/// engine-home `.env` (key rotation follows the link), copying where
/// symlinks are unavailable.
fn provision_role_env(paths: &crate::paths::Paths, role: &str) {
    if role == ka_agent_core::ADMIN_ROLE {
        return;
    }
    let src = paths.engine_home().join(".config").join("ka").join(".env");
    let dst = paths
        .role_engine_home(role)
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
        // A copy (non-unix fallback) refreshes when the profile's file
        // moved; a symlink is always current.
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

pub(super) async fn rt_on_engine_closed(sh: &Arc<Shared>, port: Arc<PortSession>) {
    port.kill().await;
    let now = ka_agent_core::now_ms();
    let cfg = cfg_of(sh);
    let window_ms = (cfg.worker_restart.crash_window_secs as i64) * 1000;
    let deliberate = {
        let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.live.as_ref().is_some_and(|l| l.deliberate_close)
    };
    let (status, note, lost_turn) = {
        let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
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
    // A crash mid-turn must say *what* died, not just that something did:
    // the prompt is still in the store, so name it (and how to re-send it).
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
    rt_persist_status(sh, status);
    sh.emit(AgentEvent::Session {
        status: format!("{status:?}"),
    });
    if deliberate {
        sh.emit(AgentEvent::Note { text: note.clone() });
    } else {
        sh.emit(AgentEvent::Notice { text: note });
    }
    // A message from another role may have waited for this engine: the
    // session is settled now, so hand the core over.
    Runtime { sh: Arc::clone(sh) }.yield_to_waiting_role().await;
}
