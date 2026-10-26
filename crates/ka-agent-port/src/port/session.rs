//! The subprocess worker session: spawn, the reader/stderr tasks and
//! process-group kill. Split from port.rs (moves only).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::{Mutex, oneshot};

use super::events::{PortEvent, map_ka_event};
use super::{OpenRequest, StrandSelector};
use crate::proto::{ImagePayload, WorkerCommand, WorkerEvent};

/// How long `open` waits for the worker's `ready` line.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `close` waits for a clean worker exit before killing.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
/// Capacity of the port event stream (mirrors ka's engine event channel).
const EVENT_CHANNEL_CAPACITY: usize = 256;
/// Bounded tail of worker stderr kept for diagnostics (`stderr_tail`).
const STDERR_RING: usize = 256;

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
    pub(crate) async fn spawn(req: OpenRequest) -> Result<Self, super::PortError> {
        use super::PortError;

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
        // included) is killable as one unit (ka-port.md §8). Windows has
        // no process groups; the kill there targets the process itself.
        #[cfg(unix)]
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
                auto_allow: req.auto_allow,
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
    ) -> Result<(), super::PortError> {
        self.submit_with_model(text, images, skills, None).await
    }

    /// Submit with a per-turn model override (background turns).
    pub async fn submit_with_model(
        &self,
        text: String,
        images: Vec<ImagePayload>,
        skills: Vec<String>,
        model: Option<String>,
    ) -> Result<(), super::PortError> {
        self.send(WorkerCommand::Submit {
            text,
            model,
            images,
            skills,
        })
        .await
    }

    /// Answer a pending ask (`choice` indexes the ask's options).
    pub async fn answer(&self, ask_id: String, choice: usize) -> Result<(), super::PortError> {
        self.send(WorkerCommand::Answer { ask_id, choice }).await
    }

    /// Abort the running cycle; expect `TurnFinished{aborted}` + `Idle`.
    pub async fn abort(&self) -> Result<(), super::PortError> {
        self.send(WorkerCommand::Abort).await
    }

    /// Switch the model.
    pub async fn set_model(&self, selector: String) -> Result<(), super::PortError> {
        self.send(WorkerCommand::SetModel { selector }).await
    }

    /// Switch the permission mode.
    pub async fn set_mode(&self, mode: String) -> Result<(), super::PortError> {
        self.send(WorkerCommand::SetMode { mode }).await
    }

    /// Set the per-turn reasoning effort ("" clears it).
    pub async fn set_effort(&self, level: String) -> Result<(), super::PortError> {
        self.send(WorkerCommand::SetEffort { level }).await
    }

    /// Steer the live turn (delivered at the next tool-batch boundary).
    pub async fn steer(&self, text: String) -> Result<(), super::PortError> {
        self.send(WorkerCommand::Steer { text }).await
    }

    /// Undo the most recent file change of the session.
    pub async fn undo_file(&self) -> Result<(), super::PortError> {
        self.send(WorkerCommand::UndoFile).await
    }

    /// Name (title) the session.
    pub async fn set_name(&self, name: String) -> Result<(), super::PortError> {
        self.send(WorkerCommand::SetName { name }).await
    }

    /// Compact the strand into a digest.
    pub async fn compact(&self) -> Result<(), super::PortError> {
        self.send(WorkerCommand::Compact).await
    }

    /// Graceful stop: send `close`, wait up to 3 s for the worker to
    /// exit, then escalate to a group kill.
    pub async fn close(&self) -> Result<(), super::PortError> {
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
            Err(super::PortError::EngineClosed) => Ok(()),
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

    async fn send(&self, cmd: WorkerCommand) -> Result<(), super::PortError> {
        if self.state.dead.load(Ordering::SeqCst) {
            return Err(super::PortError::EngineClosed);
        }
        if self.state.closed.load(Ordering::SeqCst) {
            return Err(super::PortError::Closed);
        }
        self.write_line(&cmd).await
    }

    async fn write_line(&self, cmd: &WorkerCommand) -> Result<(), super::PortError> {
        let line = crate::proto::to_line(cmd)
            .map_err(|e| super::PortError::Protocol(format!("serialize {cmd:?}: {e}")))?;
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(super::PortError::Closed);
        };
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|_| super::PortError::EngineClosed)?;
        stdin
            .flush()
            .await
            .map_err(|_| super::PortError::EngineClosed)
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
                    && tx.send(pe).await.is_err()
                {
                    break; // consumer gone
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

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
}
