// MatrixSurface: the outbound Surface impl and sender queue. Split from matrix.rs (moves only).

use super::MatrixConfig;
use super::client::Client;
use crate::commands;
use ka_agent_core::ApprovalStatus;
use ka_agent_runtime::surface::{ApprovalCard, Surface};
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
    /// In-turn progress: never sent (no message edits on Matrix).
    fn on_note(&self, _text: &str) {}

    fn on_notice(&self, text: &str) {
        self.enqueue(text);
    }

    fn on_approval(&self, card: &ApprovalCard) {
        let left = (card.expires_at_ms - ka_agent_core::now_ms()).max(0);
        let text = format!(
            "approval {} — {}\n\nexpires in {}\n/y to allow · /n to deny",
            ka_agent_core::ids::short_id(&card.approval_id),
            card.question,
            commands::fmt_interval(left)
        );
        self.enqueue(&text);
    }

    fn on_approval_settled(&self, approval_id: &str, status: ApprovalStatus) {
        self.enqueue(&format!(
            "approval {} → {status:?}",
            ka_agent_core::ids::short_id(approval_id)
        ));
    }

    fn on_final_chunk(&self, _index: u32, text: &str) {
        self.enqueue(text);
    }

    fn on_error(&self, class: &str, message: &str) {
        self.enqueue(&format!("! [{class}] {message}"));
    }

    /// Cost and token counts live in `/status`; nothing is posted.
    fn on_turn_finished(&self, _stop: &str, _cost_usd: f64, _tokens: u64) {}

    /// Session state changes are not posted (pauses arrive as notices).
    fn on_session_status(&self, _status: ka_agent_core::SessionStatus) {}
}
