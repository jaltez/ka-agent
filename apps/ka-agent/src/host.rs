//! The runtime host: one composition root shared by the daemon
//! (`ka-agent run`, channels + web + admin socket) and the local chat
//! (`ka-agent chat` when no daemon runs — same runtime, same admin socket,
//! no phone channels, no web).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ka_agent_core::DaemonConfig;
use ka_agent_runtime::{
    Envelope, EventBus, InstanceLock, LockError, Runtime, Surface, paths::Paths,
};
use ka_agent_store::Store;
use tokio::sync::broadcast;

use crate::daemon::LogSurface;
use crate::identity::migrate_config_identities;
use crate::ipc;

/// What a host runs: phone channels and web (the daemon), or neither (the
/// local chat — it serves the IPC admin endpoint either way).
#[derive(Default)]
pub struct HostOptions {
    /// Telegram/Matrix pollers, their tokens and the voice transcriber.
    pub channels: bool,
    /// Web UI port override (`[web]` config still applies when `Some`).
    pub web_port: Option<u16>,
    /// Scripted engine turns (e2e).
    pub scenario: Option<PathBuf>,
}

/// A running composition: runtime, store, bus, spawned tasks and the
/// instance lock that proves ownership of the data root.
pub struct Host {
    pub runtime: Runtime,
    pub store: Arc<Store>,
    pub bus: Arc<EventBus>,
    shutdown: tokio::sync::watch::Sender<bool>,
    _lock: InstanceLock,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    sock_path: PathBuf,
    /// Subscribed before the runtime existed (its recovery notices count);
    /// the local chat takes it, the daemon never does.
    early_rx: Option<broadcast::Receiver<std::sync::Arc<Envelope>>>,
}

impl Host {
    /// A receiver for the host's shutdown signal (the admin `shutdown` op
    /// fires it; the daemon waits on it next to SIGINT/SIGTERM).
    pub fn shutdown_rx(&self) -> tokio::sync::watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    /// The bus receiver subscribed before the runtime existed. Taken once,
    pub fn take_early_rx(&mut self) -> Option<broadcast::Receiver<std::sync::Arc<Envelope>>> {
        self.early_rx.take()
    }

    /// Bring up everything a host runs. Every failure prints exactly what
    /// the daemon printed at that point and returns it as the error.
    pub fn start(config: &Path, opts: HostOptions) -> Result<Host, String> {
        // Strict, like every long-running path: a host with a broken config
        // must not silently run with defaults.
        let cfg = DaemonConfig::load(config).map_err(|e| crate::config_problem(config, &e))?;
        let paths = Paths::new(Paths::default_root());
        paths
            .ensure_dirs()
            .map_err(|e| format!("ka-agent: cannot create {}: {e}", paths.root().display()))?;
        // One runtime per data root: held for the whole host. Only after
        // acquiring it is a leftover admin socket provably stale.
        let lock = match InstanceLock::acquire(&paths) {
            Ok(l) => l,
            Err(LockError::Held(root)) => {
                return Err(crate::daemon::lock_held_text(&root));
            }
            Err(LockError::Io(e)) => {
                return Err(format!(
                    "ka-agent: cannot lock {}: {e}",
                    paths.instance_lock().display()
                ));
            }
        };
        let store = Store::open(&paths.state_db())
            .map(Arc::new)
            .map_err(|e| format!("ka-agent: cannot open {}: {e}", paths.state_db().display()))?;
        // The deprecated config allowlist migrates into the identities table
        // (§8.1): pairing is CLI-only now.
        let migrated = migrate_config_identities(&store, &cfg);
        if migrated > 0 {
            println!(
                "migrated {migrated} telegram.allow_user_ids entr(ies) into identities \
                 — remove the key from ka-agent.toml; manage links with `ka-agent identity`"
            );
        }

        let mut tasks = Vec::new();

        // Telegram token, resolved once (surface and channel share it).
        let token = if opts.channels && cfg.telegram.enabled {
            let secret_name = cfg
                .telegram
                .token_secret
                .clone()
                .unwrap_or_else(|| "telegram.bot_token".to_string());
            match ka_agent_core::secret::resolve(&secret_name, &paths.root().join("secrets")) {
                Ok(t) => Some(t),
                Err(e) => {
                    return Err(format!(
                        "ka-agent: cannot resolve telegram token {secret_name:?}: {e}\n\
                         set KA_AGENT_SECRET_TELEGRAM_BOT_TOKEN or run: \
                         ka-agent secret set {secret_name}"
                    ));
                }
            }
        } else {
            None
        };
        // Matrix token (resolved once; the surface and channel share it).
        let matrix_token = if opts.channels && cfg.matrix.enabled {
            let secret_name = cfg
                .matrix
                .token_secret
                .clone()
                .unwrap_or_else(|| "matrix.access_token".to_string());
            match ka_agent_core::secret::resolve(&secret_name, &paths.root().join("secrets")) {
                Ok(t) => Some(t),
                Err(e) => {
                    return Err(format!(
                        "ka-agent: cannot resolve matrix token {secret_name:?}: {e}\n\
                         set KA_AGENT_SECRET_MATRIX_ACCESS_TOKEN or run: \
                         ka-agent secret set {secret_name}"
                    ));
                }
            }
        } else {
            None
        };
        // Surfaces fan out: the journal (daemon), the event bus (attached
        // terminals, the web chat, and the local chat's own reader), and
        // every enabled channel.
        let bus = Arc::new(EventBus::new());
        // Subscribe before the runtime exists: its recovery notices count.
        let early_rx = bus.subscribe();
        let mut surfaces: Vec<Arc<dyn Surface>> = if opts.channels {
            vec![Arc::new(LogSurface), bus.clone()]
        } else {
            vec![bus.clone()]
        };
        let _telegram_surface = token.as_ref().map(|t| {
            // The operator's last chat survives restarts, so reminders and
            // cron output arrive before anyone writes again.
            let initial_chat = store
                .meta_get("telegram.chat_id")
                .ok()
                .flatten()
                .and_then(|s| s.parse::<i64>().ok());
            let s: Arc<ka_agent_channels::TelegramSurface> =
                Arc::new(ka_agent_channels::TelegramSurface::new(
                    cfg.telegram.api_base().to_string(),
                    t.clone(),
                    initial_chat,
                ));
            s.set_streaming(cfg.telegram.stream);
            surfaces.push(s.clone());
            s
        });
        let matrix_surface = matrix_token.as_ref().map(|t| {
            let homeserver = cfg.matrix.homeserver.clone().unwrap_or_default();
            // The last room the operator wrote from survives restarts, so
            // proactive notices are delivered before anyone writes again.
            let initial_room = store.meta_get("matrix.room_id").ok().flatten();
            let s: Arc<ka_agent_channels::MatrixSurface> =
                Arc::new(ka_agent_channels::MatrixSurface::new(
                    ka_agent_channels::MatrixConfig {
                        homeserver,
                        user_id: cfg.matrix.user_id.clone(),
                        token: t.clone(),
                        transcriber: None,
                    },
                    initial_room,
                ));
            surfaces.push(s.clone());
            s
        });
        let surface: Arc<dyn Surface> = Arc::new(ka_agent_channels::MultiSurface::new(surfaces));

        let mut port = ka_agent_port::WorkerPort::with_current_exe();
        if let Some(sc) = opts.scenario {
            port.scenario_override = Some(sc);
        }
        let runtime = Runtime::new(
            cfg.clone(),
            paths.clone(),
            Arc::clone(&store),
            port,
            Arc::clone(&surface),
        )
        .map_err(|e| format!("ka-agent: runtime init failed: {e}"))?;
        runtime.set_config_path(config.to_path_buf());
        // No-agent cron handlers (M11): the calendar digest runs without the
        // engine. Registered right after the runtime exists, before any tick.
        {
            let st = Arc::clone(&store);
            let tz = cfg.time_zone();
            runtime.set_no_agent_handler(
                "calendar",
                Arc::new(move |src| ka_agent_channels::calendar::digest_in(src, &st, &tz)),
            );
        }
        if opts.channels {
            println!(
                "ka-agent run · session {} · data {}",
                runtime.session_id(),
                paths.root().display()
            );
        }

        let maintenance = runtime.clone();
        tasks.push(tokio::spawn(async move {
            maintenance
                .run_maintenance(std::time::Duration::from_secs(15))
                .await;
        }));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Voice-note transcription for the phone channels (key from the
        // engine `.env` tier, like every provider key).
        let transcriber = if opts.channels {
            cfg.voice.as_ref().and_then(|voice| {
                let env = ka_agent_port::catalog::read_env_file(
                    &ka_agent_port::catalog::engine_env_path(&paths.engine_home()),
                );
                ka_agent_channels::voice::Transcriber::from_config(voice, &env)
                    .map_err(|e| eprintln!("ka-agent: {e} (voice notes off)"))
                    .ok()
            })
        } else {
            None
        };

        // One bot token, one poller: refuse before the fight starts (the
        // provider would answer 409 and the two daemons would steal updates).
        if opts.channels {
            let base = ka_agent_core::layout::base_dir();
            if let Some(t) = &token
                && let Err(e) = ka_agent_port::token_lock::acquire(
                    "telegram",
                    t,
                    &ka_agent_core::layout::profile_name(),
                    &base,
                )
            {
                return Err(format!("ka-agent: {e}"));
            }
            if let Some(t) = &matrix_token
                && let Err(e) = ka_agent_port::token_lock::acquire(
                    "matrix",
                    t,
                    &ka_agent_core::layout::profile_name(),
                    &base,
                )
            {
                return Err(format!("ka-agent: {e}"));
            }
        }
        if let Some(token) = token {
            let mut tcfg = ka_agent_channels::TelegramConfig::from_config(&cfg, token);
            tcfg.transcriber = transcriber.clone();
            let channel =
                ka_agent_channels::TelegramChannel::new(tcfg, runtime.clone(), Arc::clone(&store));
            tasks.push(tokio::spawn(channel.run(shutdown_rx.clone())));
        }
        if let (Some(surface), Some(token)) = (matrix_surface, matrix_token) {
            let mcfg = ka_agent_channels::MatrixConfig {
                homeserver: cfg.matrix.homeserver.clone().unwrap_or_default(),
                user_id: cfg.matrix.user_id.clone(),
                token,
                transcriber: transcriber.clone(),
            };
            let channel = ka_agent_channels::MatrixChannel::new(
                mcfg,
                runtime.clone(),
                Arc::clone(&store),
                surface,
            );
            use ka_agent_channels::Channel as _;
            tasks.push(tokio::spawn(channel.run(shutdown_rx.clone())));
            if opts.channels {
                println!(
                    "matrix channel on {}",
                    cfg.matrix.homeserver.clone().unwrap_or_default()
                );
            }
        }

        // Admin socket: newline-delimited JSON. The lock above proves a
        // leftover socket is stale.
        let listener = ipc::IpcListener::bind(&paths).map_err(|e| {
            format!(
                "ka-agent: cannot bind {}: {e}",
                ipc::socket_path(&paths).display()
            )
        })?;
        let sock_path = ipc::socket_path(&paths);
        if opts.channels {
            println!("admin socket {}", sock_path.display());
        }

        // Web UI (M7): loopback-only, session auth; `[web]` in the config,
        // or `--web-port` as an override.
        let web_port = opts
            .web_port
            .or_else(|| cfg.web.enabled.then_some(cfg.web.port));
        if let Some(web_port) = web_port {
            let web = ka_agent_web::WebServer::new(
                web_port,
                runtime.clone(),
                Arc::clone(&store),
                Arc::clone(&bus),
            );
            let web_rx = shutdown_rx.clone();
            tasks.push(tokio::spawn(async move {
                if let Err(e) = web.run(web_rx).await {
                    eprintln!("ka-agent: web UI disabled: {e}");
                }
            }));
            if opts.channels {
                println!("web UI http://127.0.0.1:{web_port} (open it with: ka-agent dashboard)");
            }
        }

        let ctx = ipc::AdminCtx {
            runtime: runtime.clone(),
            store: Arc::clone(&store),
            bus: Arc::clone(&bus),
            shutdown: shutdown_tx.clone(),
            config: config.to_path_buf(),
        };
        tasks.push(tokio::spawn(async move {
            loop {
                let Ok(stream) = listener.accept().await else {
                    continue;
                };
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = ipc::serve_admin(stream, ctx).await {
                        eprintln!("ka-agent: admin request: {e}");
                    }
                });
            }
        }));

        Ok(Host {
            runtime,
            store,
            bus,
            shutdown: shutdown_tx,
            _lock: lock,
            tasks,
            sock_path,
            early_rx: Some(early_rx),
        })
    }

    /// Stop the host: signal every spawned task, abort the acceptors, and
    /// remove the admin socket so a stopped host looks stopped (leaving it
    /// behind made `ka-agent restore` refuse after any clean stop).
    pub fn shutdown(self) {
        let _ = self.shutdown.send(true);
        for task in self.tasks {
            task.abort();
        }
        // The admin socket is created (and removed) at startup normally;
        // remove it here so a stopped host looks stopped.
        let _ = std::fs::remove_file(&self.sock_path);
    }
}
