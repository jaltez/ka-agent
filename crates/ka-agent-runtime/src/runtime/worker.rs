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
                guard_write: self.guard_write_paths(),
                guard_read: self.guard_read_paths(),
            };
            let user_layer = cfg
                .engine_layer_path()
                .and_then(|p| std::fs::read_to_string(p).ok());
            (g.session.clone(), env, user_layer)
        };
        // Ka Agent's own engine features (search, fallback, MCP servers)
        // join the operator's layer; `secret:` values resolve here, in
        // memory, from the secret store.
        let secrets_dir = self.sh.paths.root().join("secrets");
        let engine_layer_toml =
            ka_agent_port::engine_layer::compose(&cfg, engine_layer_toml.as_deref(), &|name| {
                ka_agent_core::secret::resolve(name, &secrets_dir).map_err(|e| e.to_string())
            })
            .map_err(Error::Refused)?;
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

pub(super) async fn rt_on_engine_closed(sh: &Arc<Shared>, port: Arc<PortSession>) {
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
}
