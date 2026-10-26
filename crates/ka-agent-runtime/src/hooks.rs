//! Operator hooks (plan 2.7): agent events spawn `sh -c <command>` with
//! the envelope on stdin, `KA_HOOK_EVENT` and `KA_HOOK_SUMMARY` in the env,
//! cwd = the workspace, killed at the declared timeout. Hooks observe; they
//! never block the runtime (bounded queue, at most four in flight).

use std::sync::Arc;
use std::time::Duration;

use ka_agent_core::config::{AGENT_HOOK_EVENTS, HookSpec};
use tokio::sync::{Semaphore, mpsc};

use crate::event::{AgentEvent, Envelope};
use crate::surface::Surface;

/// In-flight ceiling and queue depth: a hook that wedges must never grow
/// the runtime's memory or delay an event fan-out.
const MAX_IN_FLIGHT: usize = 4;
const QUEUE_DEPTH: usize = 64;

/// The agent event a hook may subscribe to, when this envelope maps to one.
fn event_name(ev: &AgentEvent) -> Option<&'static str> {
    match ev {
        AgentEvent::Inbound { .. } => Some("message_received"),
        AgentEvent::TurnFinished { .. } => Some("turn_finished"),
        AgentEvent::Approval { .. } => Some("approval_requested"),
        AgentEvent::ApprovalSettled { .. } => Some("approval_settled"),
        AgentEvent::Reminder { .. } => Some("reminder_fired"),
        AgentEvent::CronFinished { .. } => Some("cron_finished"),
        AgentEvent::Session { status } if status == "Paused" => Some("session_paused"),
        AgentEvent::MemoryProposal { .. } => Some("memory_proposed"),
        _ => None,
    }
}

/// A one-line human digest for `KA_HOOK_SUMMARY`.
fn summary(ev: &AgentEvent) -> String {
    let text = match ev {
        AgentEvent::Inbound { text, .. } => text.clone(),
        AgentEvent::TurnFinished { text, stop, .. } => format!("{stop}: {text}"),
        AgentEvent::Approval { question, .. } => question.clone(),
        AgentEvent::ApprovalSettled { id, status } => format!("{id} {status}"),
        AgentEvent::Reminder { text, .. } => text.clone(),
        AgentEvent::CronFinished { id, status } => format!("{id} {status}"),
        AgentEvent::Session { status } => status.clone(),
        AgentEvent::MemoryProposal { text, .. } => text.clone(),
        other => format!("{other:?}"),
    };
    let line = text.lines().next().unwrap_or("");
    line.chars().take(200).collect()
}

pub struct HookSurface {
    hooks: Vec<HookSpec>,
    workspace: std::path::PathBuf,
    tx: mpsc::Sender<(usize, Arc<Envelope>)>,
}

impl HookSurface {
    /// Build the surface and its worker task (one per host; cheap).
    pub fn start(hooks: Vec<HookSpec>, workspace: std::path::PathBuf) -> Arc<Self> {
        let hooks: Vec<HookSpec> = hooks
            .into_iter()
            .filter(|h| AGENT_HOOK_EVENTS.contains(&h.on.as_str()))
            .collect();
        let (tx, mut rx) = mpsc::channel::<(usize, Arc<Envelope>)>(QUEUE_DEPTH);
        let sem = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
        let all = hooks.clone();
        let ws = workspace.clone();
        tokio::spawn(async move {
            while let Some((idx, env)) = rx.recv().await {
                let Some(spec) = all.get(idx).cloned() else {
                    continue;
                };
                let Ok(permit) = Arc::clone(&sem).acquire_owned().await else {
                    break;
                };
                let cwd = ws.clone();
                tokio::spawn(async move {
                    run_hook(&spec, &env, &cwd).await;
                    drop(permit);
                });
            }
        });
        Arc::new(Self {
            hooks,
            workspace,
            tx,
        })
    }

    pub fn count(&self) -> usize {
        self.hooks.len()
    }
}

impl Surface for HookSurface {
    fn emit(&self, env: &Envelope) {
        let Some(name) = event_name(&env.ev) else {
            return;
        };
        for (idx, h) in self.hooks.iter().enumerate() {
            if h.on != name {
                continue;
            }
            // try_send: a full queue drops the hook run (logged), never the
            // caller.
            if self.tx.try_send((idx, Arc::new(env.clone()))).is_err() {
                eprintln!("hooks: queue full, dropped {} for {name}", h.command);
            }
        }
    }
}

impl std::fmt::Debug for HookSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookSurface")
            .field("hooks", &self.hooks.len())
            .field("workspace", &self.workspace)
            .finish()
    }
}

async fn run_hook(spec: &HookSpec, env: &Envelope, cwd: &std::path::Path) {
    let timeout = Duration::from_secs(spec.timeout_secs.unwrap_or(10).clamp(1, 60));
    let payload = serde_json::to_string(env).unwrap_or_default();
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", &spec.command]);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", &spec.command]);
        c
    };
    cmd.current_dir(if cwd.is_dir() {
        cwd
    } else {
        std::path::Path::new(".")
    })
    .env("KA_HOOK_EVENT", spec.on.clone())
    .env("KA_HOOK_SUMMARY", summary(&env.ev))
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
    let Ok(mut child) = cmd.spawn() else {
        eprintln!("hooks: cannot spawn {:?}", spec.command);
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt as _;
        let _ = stdin.write_all(payload.as_bytes()).await;
    }
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => {
            let first =
                |b: &[u8]| -> String { String::from_utf8_lossy(b).chars().take(1024).collect() };
            let out_text = first(&out.stdout);
            let err_text = first(&out.stderr);
            if !out_text.trim().is_empty() || !err_text.trim().is_empty() {
                eprintln!(
                    "hooks: {} (exit {:?}): {}{}",
                    spec.command,
                    out.status.code(),
                    out_text.trim_end(),
                    if err_text.trim().is_empty() {
                        String::new()
                    } else {
                        format!(" [stderr] {}", err_text.trim_end())
                    }
                );
            }
        }
        Ok(Err(e)) => eprintln!("hooks: {:?} failed: {e}", spec.command),
        Err(_) => eprintln!("hooks: {:?} timed out after {timeout:?}", spec.command),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn event_names_map_as_documented() {
        assert_eq!(
            event_name(&AgentEvent::Inbound {
                source: "console".into(),
                text: "x".into()
            }),
            Some("message_received")
        );
        assert_eq!(
            event_name(&AgentEvent::Session {
                status: "Paused".into()
            }),
            Some("session_paused")
        );
        assert_eq!(
            event_name(&AgentEvent::Session {
                status: "Idle".into()
            }),
            None
        );
        assert_eq!(
            summary(&AgentEvent::Approval {
                id: "a".into(),
                question: "run rm?\nsecond line".into(),
                detail: None,
                options: vec![],
                expires_at_ms: 0,
            }),
            "run rm?"
        );
    }
}
