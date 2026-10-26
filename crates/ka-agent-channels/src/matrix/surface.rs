// MatrixSurface: the outbound Surface impl and sender queue. Split from matrix.rs (moves only).

use super::MatrixConfig;
use super::client::Client;
use crate::commands;
use ka_agent_runtime::Envelope;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::surface::Surface;
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc};

/// The Matrix surface: notices, answers and approval cards go to the room
/// the operator last wrote from (one operator per instance). In-turn
/// progress notes are never sent (the journal keeps them).
pub struct MatrixSurface {
    pub(super) client: Arc<Client>,
    pub(super) room: Arc<Mutex<Option<String>>>,
    pub(super) room_known: Arc<Notify>,
    tx: mpsc::UnboundedSender<String>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
}

impl MatrixSurface {
    pub fn new(cfg: MatrixConfig, initial_room: Option<String>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            client: Arc::new(Client::new(cfg.homeserver, cfg.token)),
            room: Arc::new(Mutex::new(initial_room.filter(|r| !r.is_empty()))),
            room_known: Arc::new(Notify::new()),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    /// Take the outbound queue (once): the channel's sender task drains it,
    /// keeping sends on one task and in order.
    pub fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<String>> {
        self.rx.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Set the delivery room; returns whether it changed.
    pub(super) fn set_room(&self, room: &str) -> bool {
        let changed = {
            let mut cur = self.room.lock().unwrap_or_else(|e| e.into_inner());
            if cur.as_deref() == Some(room) {
                false
            } else {
                *cur = Some(room.to_string());
                true
            }
        };
        self.room_known.notify_one();
        changed
    }

    pub(super) fn enqueue(&self, text: &str) {
        let _ = self.tx.send(text.to_string());
    }
}

impl Surface for MatrixSurface {
    fn emit(&self, env: &Envelope) {
        match &env.ev {
            // In-turn progress: never sent (no message edits on Matrix).
            AgentEvent::Note { .. } => {}
            AgentEvent::Notice { text } => self.enqueue(text),
            AgentEvent::Approval {
                id,
                question,
                expires_at_ms,
                ..
            } => {
                let left = (expires_at_ms - ka_agent_core::now_ms()).max(0);
                let text = format!(
                    "approval {} — {}\n\nexpires in {}\n/y to allow · /n to deny",
                    ka_agent_core::ids::short_id(id),
                    question,
                    commands::fmt_interval(left)
                );
                self.enqueue(&text);
            }
            AgentEvent::ApprovalSettled { id, status } => {
                self.enqueue(&format!(
                    "approval {} → {status}",
                    ka_agent_core::ids::short_id(id)
                ));
            }
            AgentEvent::FinalChunk { text, .. } => self.enqueue(text),
            AgentEvent::Error { class, message } => {
                self.enqueue(&format!("! [{class}] {message}"));
            }
            // Cost and token counts live in `/status`; nothing is posted.
            AgentEvent::TurnFinished { .. } => {}
            // Session state changes are not posted (pauses arrive as
            // notices).
            AgentEvent::Session { .. } => {}
            AgentEvent::MemoryProposal { id, text } => {
                let short = ka_agent_core::ids::short_id(id);
                self.enqueue(&format!(
                    "💡 remember this? {text}\n   /memory accept {short} · /memory reject {short}"
                ));
            }
            // Reasoning, tool structure and streaming drafts are not drawn
            // on Matrix (today's behavior).
            AgentEvent::Thought { .. }
            | AgentEvent::ToolStarted { .. }
            | AgentEvent::ToolFinished { .. }
            | AgentEvent::Plan { .. }
            | AgentEvent::Context { .. }
            | AgentEvent::Compacting { .. }
            | AgentEvent::Inbound { .. }
            | AgentEvent::TurnStarted { .. }
            | AgentEvent::Delta { .. } => {}
            // Later steps wire these to real deliveries.
            AgentEvent::Reminder { .. }
            | AgentEvent::File { .. }
            | AgentEvent::CronFinished { .. }
            | AgentEvent::Queue { .. }
            | AgentEvent::Settings { .. }
            | AgentEvent::SkillProposal { .. } => {}
            // Per-connection events never reach a broadcast surface.
            AgentEvent::Hello { .. } | AgentEvent::Reply { .. } => {}
        }
    }
}
