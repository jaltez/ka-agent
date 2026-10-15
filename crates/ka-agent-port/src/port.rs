//! The Ka boundary: the [`KaPort`] trait and the subprocess-worker
//! client (`docs/contract/ka-port.md`).
//!
//! Everything above this crate is written against [`KaPort`] /
//! [`PortEvent`]; no ka type crosses the boundary (SEED §3.1.5). The
//! concrete implementation spawns the worker binary over NDJSON stdio
//! (`docs/contract/worker-protocol.md`), one worker per open session,
//! each in its own process group with a sanitized environment.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::{Mutex, oneshot};

use crate::proto::{ImagePayload, WorkerCommand, WorkerEvent};

/// A boxed, Send future — the return shape of [`KaPort::open`] so the
/// trait stays dyn-compatible.
pub type BoxFut<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How long `open` waits for the worker's `ready` line.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `close` waits for a clean worker exit before killing.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
/// Capacity of the port event stream (mirrors ka's engine event channel).
const EVENT_CHANNEL_CAPACITY: usize = 256;
/// Bounded tail of worker stderr kept for diagnostics (`stderr_tail`).
const STDERR_RING: usize = 256;

/// Which strand an open attaches to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrandSelector {
    /// Create a fresh strand.
    New,
    /// Resume the strand at this file path.
    Path(PathBuf),
}

/// The worker's pinned environment (SEED §3.1.5 / §6.4, validation §7):
/// the worker must never inherit the operator's interactive Ka state or
/// ambient credentials, or it silently uses the operator's model, mode
/// and keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerEnv {
    /// `HOME` for the worker (compiled `MEMORY.md`, skills).
    pub home: PathBuf,
    /// `KA_DATA_DIR` (strands, waypoints).
    pub data_dir: PathBuf,
    /// `XDG_STATE_HOME` (spills, trust store, jobs).
    pub state_dir: PathBuf,
    /// `XDG_CACHE_HOME` (feeds the sandbox allowlist).
    pub cache_dir: PathBuf,
    /// `KA_AGENT_DATA_DIR` for the worker and its spawned MCP bridge.
    pub agent_data_root: PathBuf,
}

/// Env-var name markers whose values are ambient operator credentials —
/// never allowed into a worker. Provider secrets are injected per
/// session by the runtime, not inherited.
const SECRET_MARKERS: &[&str] = &["KEY", "TOKEN", "SECRET", "PASSWORD", "AUTH"];

fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_MARKERS.iter().any(|m| upper.contains(m))
}

impl WorkerEnv {
    /// Prepare a worker [`Command`]'s environment: keep the ambient
    /// variables except credential-shaped ones (matched
    /// case-insensitively on the name), then pin the five
    /// isolation-critical variables. `XDG_CONFIG_HOME` is pinned because
    /// ka's user config layer resolves through it, bypassing `HOME`
    /// (validation finding F-2).
    pub fn sanitize(&self, cmd: &mut Command) {
        let keep: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
            .filter(|(name, _)| !name.to_str().is_some_and(is_secret_name))
            .collect();
        cmd.env_clear().envs(keep);
        cmd.env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_STATE_HOME", &self.state_dir)
            .env("XDG_CACHE_HOME", &self.cache_dir)
            .env("KA_DATA_DIR", &self.data_dir)
            .env("KA_AGENT_DATA_DIR", &self.agent_data_root);
    }
}

/// Everything one open needs.
#[derive(Debug, Clone)]
pub struct OpenRequest {
    /// Working directory (also the strand-storage key).
    pub workspace: PathBuf,
    /// Fresh strand or resume-by-path.
    pub strand: StrandSelector,
    /// Model selector re-asserted after attach.
    pub model: Option<String>,
    /// Permission mode re-asserted after attach (`guarded` default).
    pub mode: Option<String>,
    /// Ka engine layer TOML applied below the worker's security overlay.
    pub engine_layer_toml: Option<String>,
    /// Isolation environment for the worker process.
    pub env: WorkerEnv,
    /// Worker binary to spawn.
    pub binary: PathBuf,
    /// Extra argv before the worker protocol flags, e.g. `["__worker"]`
    /// when the binary is a multi-command app.
    pub worker_args: Vec<String>,
    /// Scripted-speaker scenario (tests / e2e; `None` in production).
    pub scenario: Option<PathBuf>,
}

impl Default for OpenRequest {
    fn default() -> Self {
        Self {
            workspace: PathBuf::new(),
            strand: StrandSelector::New,
            model: None,
            mode: None,
            engine_layer_toml: None,
            env: WorkerEnv::default(),
            binary: PathBuf::new(),
            worker_args: Vec::new(),
            scenario: None,
        }
    }
}

/// Port-level failures.
#[derive(Debug, thiserror::Error)]
pub enum PortError {
    /// The worker binary could not be spawned.
    #[error("failed to spawn worker `{}`: {source}", binary.display())]
    Spawn {
        /// The binary that failed.
        binary: PathBuf,
        /// The underlying spawn error.
        source: std::io::Error,
    },
    /// Worker or runtime I/O failure.
    #[error("worker i/o: {0}")]
    Io(#[from] std::io::Error),
    /// The worker violated the protocol (bad line, no `ready`, …).
    #[error("worker protocol: {0}")]
    Protocol(String),
    /// The session was closed by this side; no further sends.
    #[error("port session closed")]
    Closed,
    /// The worker or its engine died; no further sends.
    #[error("engine closed")]
    EngineClosed,
}

/// The port's projection of engine activity — ka events reduced to what
/// Ka Agent consumes (docs/contract/ka-port.md §3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PortEvent {
    /// The engine attached: replayed history length plus the strand
    /// identity (id always, path once the strand file exists on disk —
    /// ka materializes a fresh strand only on first append).
    Bootstrap {
        /// Strand id, once known.
        strand_id: Option<String>,
        /// Strand file path, once known.
        strand_path: Option<String>,
        /// Messages in the replay.
        replay_messages: usize,
    },
    /// A turn began.
    TurnStarted,
    /// Visible assistant text delta.
    DeltaText(String),
    /// Reasoning/thought delta.
    DeltaThought(String),
    /// A tool call began.
    CallStarted {
        /// Tool name.
        tool: String,
        /// Call id.
        id: String,
        /// Argument summary.
        detail: String,
    },
    /// A tool call finished.
    CallFinished {
        /// Tool name.
        tool: String,
        /// Call id.
        id: String,
        /// Whether the call succeeded.
        ok: bool,
    },
    /// A tool call produced output.
    CallOutput {
        /// Tool name.
        tool: String,
        /// Call id.
        id: String,
        /// Capped excerpt.
        excerpt: String,
        /// Whether the output is an error result.
        is_error: bool,
    },
    /// The engine asks the operator to resolve a permission question.
    Ask {
        /// Ask id (used in `answer`).
        ask_id: String,
        /// First question text.
        question: String,
        /// Rendered detail (e.g. a diff), when present.
        detail: Option<String>,
        /// The first question's options, verbatim (index-referenced in
        /// `answer`).
        options: Vec<String>,
    },
    /// A turn finished.
    TurnFinished {
        /// `done` | `length` | `aborted` | `error`.
        stop: String,
        /// USD cost of the turn.
        cost_usd: f64,
        /// input + output + cache_read + cache_write tokens.
        tokens: u64,
    },
    /// The prompt cycle settled; the engine is idle. Delivery boundary
    /// for one turn (SEED §3.7).
    Idle,
    /// Informational engine note.
    Note(String),
    /// Engine-level error report.
    EngineError {
        /// `auth` | `rate_limit` | `overflow` | `network` | `protocol` |
        /// `unsupported` | `internal` (or `fatal` for worker-level
        /// failures reported through `WorkerEvent::Fatal`).
        class: String,
        /// Whether a retry could help.
        retryable: bool,
        /// Human-readable detail.
        message: String,
    },
    /// The engine (or worker) is gone; the session is dead.
    EngineClosed,
}

/// Map one ka event to its port projection; `None` drops it. Dropped by
/// design: `session_info` (captured as strand identity), the bootstrap
/// state echo (`model_changed`, `mode_changed`, `effort_changed`,
/// `features_changed`), `inventory`, `todos`, `title`, `context_meter`,
/// `context_breakdown`, `digest_*`, call-kind deltas, and every
/// surface-specific event. Bootstrap state is verified by contract
/// tests, not streamed upward.
pub(crate) fn map_ka_event(
    ev: ka_protocol::Event,
    strand_id: Option<&str>,
    strand_path: Option<&str>,
) -> Option<PortEvent> {
    use ka_protocol::DeltaKind;
    use ka_protocol::Event as E;

    Some(match ev {
        E::Replay { messages } => PortEvent::Bootstrap {
            strand_id: strand_id.map(str::to_string),
            strand_path: strand_path.map(str::to_string),
            replay_messages: messages.len(),
        },
        E::TurnStarted { .. } => PortEvent::TurnStarted,
        E::Delta {
            kind: DeltaKind::Text(text),
        } => PortEvent::DeltaText(text),
        E::Delta {
            kind: DeltaKind::Thought(text),
        } => PortEvent::DeltaThought(text),
        E::Delta {
            kind: DeltaKind::Call { .. },
        } => return None,
        E::CallStarted { tool, id, detail } => PortEvent::CallStarted { tool, id, detail },
        E::CallFinished { tool, id, ok } => PortEvent::CallFinished { tool, id, ok },
        E::CallOutput {
            tool,
            id,
            excerpt,
            is_error,
            ..
        } => PortEvent::CallOutput {
            tool,
            id,
            excerpt,
            is_error,
        },
        E::Ask { id, questions } => {
            // first question only, but keep all its options
            let q = questions.first();
            PortEvent::Ask {
                ask_id: id.0,
                question: q.map(|q| q.text.clone()).unwrap_or_default(),
                detail: q.and_then(|q| q.detail.clone()),
                options: q.map(|q| q.options.clone()).unwrap_or_default(),
            }
        }
        E::TurnFinished { stop, usage } => PortEvent::TurnFinished {
            stop: stop_word(stop),
            cost_usd: usage.cost,
            tokens: usage.input + usage.output + usage.cache_read + usage.cache_write,
        },
        E::Idle => PortEvent::Idle,
        E::Note { message } => PortEvent::Note(message),
        E::Error {
            class,
            retryable,
            message,
        } => PortEvent::EngineError {
            class: error_class_word(class),
            retryable,
            message,
        },
        _ => return None,
    })
}

fn stop_word(stop: ka_protocol::Stop) -> String {
    match stop {
        ka_protocol::Stop::Done => "done",
        ka_protocol::Stop::Length => "length",
        ka_protocol::Stop::Aborted => "aborted",
        ka_protocol::Stop::Error => "error",
    }
    .to_string()
}

fn error_class_word(class: ka_protocol::ErrorClass) -> String {
    match class {
        ka_protocol::ErrorClass::Auth => "auth",
        ka_protocol::ErrorClass::RateLimit => "rate_limit",
        ka_protocol::ErrorClass::Overflow => "overflow",
        ka_protocol::ErrorClass::Network => "network",
        ka_protocol::ErrorClass::Protocol => "protocol",
        ka_protocol::ErrorClass::Unsupported => "unsupported",
        ka_protocol::ErrorClass::Internal => "internal",
    }
    .to_string()
}

/// The stable Ka boundary. Everything above the port crate depends on
/// this trait only; swapping the boundary implementation is a one-crate
/// change (SEED §3.4).
pub trait KaPort: Send + Sync {
    /// Start or resume one engine on one strand. Returns the live
    /// session handle with its event stream.
    fn open<'a>(&'a self, req: OpenRequest) -> BoxFut<'a, Result<PortSession, PortError>>;
}

/// Session-shared state between the API surface and the reader/stderr
/// tasks.
#[derive(Debug, Default)]
struct SessionState {
    strand_id: StdMutex<Option<String>>,
    strand_path: StdMutex<Option<String>>,
    /// Worker/engine dead (EngineClosed event, Fatal, or EOF).
    dead: AtomicBool,
    /// `close()` completed or `kill()` ran.
    closed: AtomicBool,
    /// The worker process has exited.
    exited: AtomicBool,
    /// Bounded stderr tail for diagnostics.
    stderr: StdMutex<VecDeque<String>>,
}

impl SessionState {
    fn snapshot_strand(&self) -> (Option<String>, Option<String>) {
        let id = self.strand_id.lock().ok().and_then(|g| g.clone());
        let path = self.strand_path.lock().ok().and_then(|g| g.clone());
        (id, path)
    }
}

/// A live worker session: one spawned worker process hosting one engine.
///
/// The event stream is taken once ([`events`](Self::events) /
/// [`take_events`](Self::take_events)); a second take returns a closed
/// receiver. Sends after death fail with [`PortError::EngineClosed`];
/// sends after [`close`](Self::close) fail with [`PortError::Closed`]
/// (unless the worker already died — dead dominates).
pub struct PortSession {
    state: Arc<SessionState>,
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Option<Child>>,
    events: StdMutex<Option<Receiver<PortEvent>>>,
}

impl PortSession {
    /// Spawn the worker, send `open`, and wait for `ready`.
    pub(crate) async fn spawn(req: OpenRequest) -> Result<Self, PortError> {
        let mut cmd = Command::new(&req.binary);
        cmd.args(&req.worker_args);
        if let Some(scenario) = &req.scenario {
            cmd.arg("--scenario").arg(scenario);
        }
        req.env.sanitize(&mut cmd);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(
                if std::env::var_os("KA_PORT_WORKER_INHERIT_STDERR").is_some() {
                    std::process::Stdio::inherit()
                } else {
                    std::process::Stdio::piped()
                },
            );
        // Own process group: the whole worker tree (engine tool children
        // included) is killable as one unit (ka-port.md §8).
        cmd.process_group(0);
        // Safety net: a dropped session must not leak a live worker.
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|source| PortError::Spawn {
            binary: req.binary.clone(),
            source,
        })?;
        let missing_pipe = |what: &str| PortError::Spawn {
            binary: req.binary.clone(),
            source: std::io::Error::new(std::io::ErrorKind::BrokenPipe, what),
        };
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| missing_pipe("worker stdin not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| missing_pipe("worker stdout not piped"))?;
        let stderr = child.stderr.take();

        let state = Arc::new(SessionState::default());
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (ready_tx, ready_rx) = oneshot::channel();
        tokio::spawn(reader_task(
            BufReader::new(stdout).lines(),
            events_tx,
            Arc::clone(&state),
            Some(ready_tx),
        ));
        if let Some(stderr) = stderr {
            tokio::spawn(stderr_task(
                BufReader::new(stderr).lines(),
                Arc::clone(&state),
            ));
        }

        let session = PortSession {
            state: Arc::clone(&state),
            stdin: Mutex::new(Some(stdin)),
            child: Mutex::new(Some(child)),
            events: StdMutex::new(Some(events_rx)),
        };

        session
            .send(WorkerCommand::Open {
                workspace: req.workspace,
                strand: match req.strand {
                    StrandSelector::New => crate::proto::StrandSel::New,
                    StrandSelector::Path(p) => {
                        crate::proto::StrandSel::Path(p.to_string_lossy().into_owned())
                    }
                },
                model: req.model,
                mode: req.mode,
                engine_layer_toml: req.engine_layer_toml,
                scenario: req.scenario,
            })
            .await?;

        match tokio::time::timeout(READY_TIMEOUT, ready_rx).await {
            Ok(Ok(())) => Ok(session),
            // the reader task ended before `ready` — the worker died at
            // startup
            Ok(Err(_)) => {
                session.kill().await;
                Err(PortError::EngineClosed)
            }
            Err(_) => {
                session.kill().await;
                Err(PortError::Protocol(
                    "worker did not report ready in time".to_string(),
                ))
            }
        }
    }

    /// The session's event stream (take-once; a second call returns a
    /// closed receiver).
    pub fn events(&self) -> Receiver<PortEvent> {
        self.take_events()
    }

    /// The session's event stream (take-once; a second call returns a
    /// closed receiver).
    pub fn take_events(&self) -> Receiver<PortEvent> {
        self.events
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .unwrap_or_else(|| mpsc::channel(1).1)
    }

    /// The active strand id, once announced.
    pub fn strand_id(&self) -> Option<String> {
        self.state.snapshot_strand().0
    }

    /// The active strand file path, once the file exists on disk.
    pub fn strand_path(&self) -> Option<String> {
        self.state.snapshot_strand().1
    }

    /// True once the worker process has exited or `close()` completed.
    pub fn is_closed(&self) -> bool {
        self.state.exited.load(Ordering::SeqCst) || self.state.closed.load(Ordering::SeqCst)
    }

    /// The bounded tail of worker stderr lines (diagnostics; the worker
    /// never speaks protocol there).
    pub fn stderr_tail(&self) -> Vec<String> {
        self.state
            .stderr
            .lock()
            .map(|ring| ring.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Submit one prompt. Single-writer rule: exactly one submit may be
    /// in flight per session (ka silently drops mid-cycle prompts, gap
    /// G4) — the runtime's queue enforces that above the port.
    pub async fn submit(
        &self,
        text: String,
        images: Vec<ImagePayload>,
        skills: Vec<String>,
    ) -> Result<(), PortError> {
        self.send(WorkerCommand::Submit {
            text,
            images,
            skills,
        })
        .await
    }

    /// Answer a pending ask (`choice` indexes the ask's options).
    pub async fn answer(&self, ask_id: String, choice: usize) -> Result<(), PortError> {
        self.send(WorkerCommand::Answer { ask_id, choice }).await
    }

    /// Abort the running cycle; expect `TurnFinished{aborted}` + `Idle`.
    pub async fn abort(&self) -> Result<(), PortError> {
        self.send(WorkerCommand::Abort).await
    }

    /// Switch the model.
    pub async fn set_model(&self, selector: String) -> Result<(), PortError> {
        self.send(WorkerCommand::SetModel { selector }).await
    }

    /// Switch the permission mode.
    pub async fn set_mode(&self, mode: String) -> Result<(), PortError> {
        self.send(WorkerCommand::SetMode { mode }).await
    }

    /// Name (title) the session.
    pub async fn set_name(&self, name: String) -> Result<(), PortError> {
        self.send(WorkerCommand::SetName { name }).await
    }

    /// Compact the strand into a digest.
    pub async fn compact(&self) -> Result<(), PortError> {
        self.send(WorkerCommand::Compact).await
    }

    /// Graceful stop: send `close`, wait up to 3 s for the worker to
    /// exit, then escalate to a group kill.
    pub async fn close(&self) -> Result<(), PortError> {
        if self.state.closed.swap(true, Ordering::SeqCst) {
            return Ok(()); // already closed from this side
        }
        let sent = self.write_line(&WorkerCommand::Close).await;
        let child = self.child.lock().await.take();
        if let Some(mut child) = child {
            if tokio::time::timeout(CLOSE_TIMEOUT, child.wait())
                .await
                .is_err()
            {
                kill_process_group(child.id()).await;
                let _ = child.start_kill();
                let _ = tokio::time::timeout(CLOSE_TIMEOUT, child.wait()).await;
            }
            self.state.exited.store(true, Ordering::SeqCst);
        }
        // drop our stdin so a worker that missed the `close` line sees EOF
        *self.stdin.lock().await = None;
        match sent {
            Ok(()) => Ok(()),
            // worker already gone — close still succeeded
            Err(PortError::EngineClosed) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Hard stop: SIGKILL the worker's whole process group (the worker
    /// is its own group leader). The event stream yields
    /// [`PortEvent::EngineClosed`]; later sends fail.
    pub async fn kill(&self) {
        self.state.closed.store(true, Ordering::SeqCst);
        self.state.dead.store(true, Ordering::SeqCst);
        let child = self.child.lock().await.take();
        if let Some(mut child) = child {
            kill_process_group(child.id()).await;
            let _ = child.start_kill();
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, child.wait()).await;
        }
        self.state.exited.store(true, Ordering::SeqCst);
        *self.stdin.lock().await = None;
    }

    async fn send(&self, cmd: WorkerCommand) -> Result<(), PortError> {
        if self.state.dead.load(Ordering::SeqCst) {
            return Err(PortError::EngineClosed);
        }
        if self.state.closed.load(Ordering::SeqCst) {
            return Err(PortError::Closed);
        }
        self.write_line(&cmd).await
    }

    async fn write_line(&self, cmd: &WorkerCommand) -> Result<(), PortError> {
        let line = crate::proto::to_line(cmd)
            .map_err(|e| PortError::Protocol(format!("serialize {cmd:?}: {e}")))?;
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(PortError::Closed);
        };
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|_| PortError::EngineClosed)?;
        stdin.flush().await.map_err(|_| PortError::EngineClosed)
    }
}

/// Parse worker stdout lines into the port event stream.
async fn reader_task(
    mut lines: tokio::io::Lines<BufReader<ChildStdout>>,
    tx: Sender<PortEvent>,
    state: Arc<SessionState>,
    mut ready: Option<oneshot::Sender<()>>,
) {
    let mut engine_closed_seen = false;
    while let Ok(Some(line)) = lines.next_line().await {
        // Lenient decode: unknown event types are skipped, never fatal
        // (ka gap G7 — closed enums, no version field).
        let Ok(ev) = serde_json::from_str::<WorkerEvent>(&line) else {
            continue;
        };
        match ev {
            WorkerEvent::Ready => {
                if let Some(sender) = ready.take() {
                    let _ = sender.send(());
                }
            }
            WorkerEvent::Strand { id, path } => {
                if let (Ok(mut id_slot), Ok(mut path_slot)) =
                    (state.strand_id.lock(), state.strand_path.lock())
                {
                    *id_slot = Some(id);
                    *path_slot = path;
                }
            }
            WorkerEvent::Ka { event } => {
                let (strand_id, strand_path) = state.snapshot_strand();
                if let Some(pe) = map_ka_event(event, strand_id.as_deref(), strand_path.as_deref())
                {
                    if tx.send(pe).await.is_err() {
                        break; // consumer gone
                    }
                }
            }
            WorkerEvent::EngineClosed { .. } => {
                engine_closed_seen = true;
                state.dead.store(true, Ordering::SeqCst);
                state.exited.store(true, Ordering::SeqCst);
                let _ = tx.send(PortEvent::EngineClosed).await;
            }
            WorkerEvent::Fatal { message } => {
                state.dead.store(true, Ordering::SeqCst);
                let _ = tx
                    .send(PortEvent::EngineError {
                        class: "fatal".to_string(),
                        retryable: false,
                        message,
                    })
                    .await;
            }
        }
    }
    // EOF: the worker process is gone. Without an explicit
    // EngineClosed line this is an unexpected death (crash, signal) —
    // surface it and mark the session dead.
    state.exited.store(true, Ordering::SeqCst);
    if !engine_closed_seen {
        state.dead.store(true, Ordering::SeqCst);
        let _ = tx.send(PortEvent::EngineClosed).await;
    }
}

/// Drain worker stderr into the bounded diagnostics ring so a chatty
/// worker can never block on a full pipe.
async fn stderr_task(
    mut lines: tokio::io::Lines<BufReader<tokio::process::ChildStderr>>,
    state: Arc<SessionState>,
) {
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(mut ring) = state.stderr.lock() {
            ring.push_back(line);
            while ring.len() > STDERR_RING {
                ring.pop_front();
            }
        }
    }
}

/// SIGKILL a worker's process group. `unsafe_code` is forbidden in this
/// workspace, so `libc::kill(2)` is out; `/bin/kill` signals the group
/// via `-<pgid>` (the worker is its own group leader, pgid == pid).
async fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = Command::new("/bin/kill")
            .args(["-s", "KILL", "--", &format!("-{pid}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
}

/// The production port: spawn the worker binary per open.
#[derive(Debug, Clone)]
pub struct WorkerPort {
    /// Default worker binary (used when an [`OpenRequest`] names none).
    pub binary: PathBuf,
    /// When set, forced into every open's `scenario` field (console chat
    /// e2e; `None` in production).
    pub scenario_override: Option<PathBuf>,
}

impl WorkerPort {
    /// A port defaulting to the current executable (the daemon binary
    /// hosting `__worker` subcommands).
    pub fn with_current_exe() -> Self {
        Self {
            binary: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ka-agent")),
            scenario_override: None,
        }
    }
}

impl Default for WorkerPort {
    fn default() -> Self {
        Self {
            binary: PathBuf::new(),
            scenario_override: None,
        }
    }
}

impl KaPort for WorkerPort {
    fn open<'a>(&'a self, mut req: OpenRequest) -> BoxFut<'a, Result<PortSession, PortError>> {
        Box::pin(async move {
            if req.binary.as_os_str().is_empty() {
                req.binary = self.binary.clone();
            }
            if let Some(scenario) = &self.scenario_override {
                req.scenario = Some(scenario.clone());
            }
            PortSession::spawn(req).await
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ka_protocol::{AskQuestion, DeltaKind, Event, Stop, Usage};

    #[test]
    fn secret_name_filter_matches_markers_case_insensitively() {
        for name in [
            "ANTHROPIC_API_KEY",
            "Zai_Api_Key",
            "GITHUB_TOKEN",
            "SECRET_CODE",
            "MY_PASSWORD",
            "SSH_AUTH_SOCK",
            "AUTHORIZATION",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in ["PATH", "HOME", "LANG", "TERM", "KA_DATA_DIR", "MODELS"] {
            assert!(!is_secret_name(name), "{name}");
        }
    }

    #[tokio::test]
    async fn sanitize_pins_isolation_and_strips_credentials() {
        let root = std::env::temp_dir().join(format!("ka-port-sanitize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        let env = WorkerEnv {
            home: root.join("home"),
            data_dir: root.join("ka"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            agent_data_root: root.clone(),
        };
        let mut cmd = Command::new("env");
        env.sanitize(&mut cmd);
        // ambient credentials visible to the test process must not survive
        let output = cmd.output().await.unwrap().stdout;
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains(&format!("HOME={}\n", root.join("home").display())),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "XDG_CONFIG_HOME={}\n",
                root.join("home/.config").display()
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "XDG_STATE_HOME={}\n",
                root.join("state").display()
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "XDG_CACHE_HOME={}\n",
                root.join("cache").display()
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!("KA_DATA_DIR={}\n", root.join("ka").display())),
            "{text}"
        );
        assert!(!text.contains("ANTHROPIC_API_KEY"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn replay_maps_to_bootstrap_with_strand_identity() {
        let ev = Event::Replay {
            messages: vec![
                ka_protocol::ReplayedMessage {
                    role: "user".into(),
                    content: "a".into(),
                    digest: false,
                    thinking: None,
                    calls: Vec::new(),
                };
                3
            ],
        };
        assert_eq!(
            map_ka_event(ev, Some("s1-x"), Some("/strands/s1-x.jsonl")),
            Some(PortEvent::Bootstrap {
                strand_id: Some("s1-x".into()),
                strand_path: Some("/strands/s1-x.jsonl".into()),
                replay_messages: 3,
            })
        );
    }

    #[test]
    fn deltas_and_calls_map_one_to_one() {
        assert_eq!(
            map_ka_event(
                Event::Delta {
                    kind: DeltaKind::Text("hi".into()),
                },
                None,
                None
            ),
            Some(PortEvent::DeltaText("hi".into()))
        );
        assert_eq!(
            map_ka_event(
                Event::Delta {
                    kind: DeltaKind::Thought("hm".into()),
                },
                None,
                None
            ),
            Some(PortEvent::DeltaThought("hm".into()))
        );
        assert_eq!(
            map_ka_event(
                Event::Delta {
                    kind: DeltaKind::Call {
                        tool: "bash".into(),
                        id: "c1".into(),
                    },
                },
                None,
                None
            ),
            None,
            "call-kind deltas are dropped (CallStarted carries them)"
        );
        assert_eq!(
            map_ka_event(
                Event::CallStarted {
                    tool: "bash".into(),
                    id: "c1".into(),
                    detail: "touch x".into(),
                },
                None,
                None
            ),
            Some(PortEvent::CallStarted {
                tool: "bash".into(),
                id: "c1".into(),
                detail: "touch x".into(),
            })
        );
        assert_eq!(
            map_ka_event(
                Event::CallFinished {
                    tool: "bash".into(),
                    id: "c1".into(),
                    ok: false,
                },
                None,
                None
            ),
            Some(PortEvent::CallFinished {
                tool: "bash".into(),
                id: "c1".into(),
                ok: false,
            })
        );
        assert_eq!(
            map_ka_event(
                Event::CallOutput {
                    tool: "bash".into(),
                    id: "c1".into(),
                    excerpt: "done".into(),
                    is_error: true,
                    spill: None,
                },
                None,
                None
            ),
            Some(PortEvent::CallOutput {
                tool: "bash".into(),
                id: "c1".into(),
                excerpt: "done".into(),
                is_error: true,
            })
        );
    }

    #[test]
    fn ask_maps_first_question_with_all_options() {
        let ev = Event::Ask {
            id: ka_protocol::AskId("ask-3".into()),
            questions: vec![
                AskQuestion {
                    text: "run it?".into(),
                    options: vec!["allow".into(), "always".into(), "deny".into()],
                    detail: Some("touch x".into()),
                },
                AskQuestion {
                    text: "second question dropped".into(),
                    options: vec!["no".into()],
                    detail: None,
                },
            ],
        };
        assert_eq!(
            map_ka_event(ev, None, None),
            Some(PortEvent::Ask {
                ask_id: "ask-3".into(),
                question: "run it?".into(),
                detail: Some("touch x".into()),
                options: vec!["allow".into(), "always".into(), "deny".into()],
            })
        );
    }

    #[test]
    fn turn_finished_sums_tokens_and_keeps_cost() {
        let ev = Event::TurnFinished {
            stop: Stop::Aborted,
            usage: Usage {
                input: 3,
                output: 5,
                cache_read: 7,
                cache_write: 11,
                cost: 0.5,
            },
        };
        assert_eq!(
            map_ka_event(ev, None, None),
            Some(PortEvent::TurnFinished {
                stop: "aborted".into(),
                cost_usd: 0.5,
                tokens: 26,
            })
        );
    }

    #[test]
    fn note_idle_and_error_map_directly() {
        assert_eq!(map_ka_event(Event::Idle, None, None), Some(PortEvent::Idle));
        assert_eq!(
            map_ka_event(
                Event::Note {
                    message: "↩ recovered interrupted turn".into(),
                },
                None,
                None
            ),
            Some(PortEvent::Note("↩ recovered interrupted turn".into()))
        );
        assert_eq!(
            map_ka_event(
                Event::Error {
                    class: ka_protocol::ErrorClass::RateLimit,
                    retryable: true,
                    message: "slow down".into(),
                },
                None,
                None
            ),
            Some(PortEvent::EngineError {
                class: "rate_limit".into(),
                retryable: true,
                message: "slow down".into(),
            })
        );
    }

    #[test]
    fn unmapped_events_are_dropped() {
        for ev in [
            Event::SessionInfo { id: "s1".into() },
            Event::ModelChanged {
                selector: "a/b".into(),
            },
            Event::ModeChanged {
                mode: ka_protocol::Mode::Plan,
            },
            Event::Title { title: "t".into() },
            Event::ContextMeter { used: 1, window: 2 },
            Event::DigestStarted,
        ] {
            assert_eq!(map_ka_event(ev, None, None), None);
        }
    }

    #[test]
    fn take_events_is_take_once() {
        let (tx, first) = mpsc::channel::<PortEvent>(4);
        let state = Arc::new(SessionState::default());
        let session = PortSession {
            state,
            stdin: Mutex::new(None),
            child: Mutex::new(None),
            events: StdMutex::new(Some(first)),
        };
        let mut second = session.take_events();
        let mut third = session.events();
        drop(tx);
        // all receivers closed once the sender is gone
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            assert!(second.recv().await.is_none());
            assert!(third.recv().await.is_none());
        });
    }

    #[test]
    fn with_current_exe_points_at_the_running_binary() {
        let port = WorkerPort::with_current_exe();
        assert_eq!(port.binary, std::env::current_exe().unwrap());
        assert!(port.scenario_override.is_none());
    }
}
