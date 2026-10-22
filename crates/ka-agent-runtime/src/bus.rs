//! The in-process event bus: every runtime surface event, broadcast to any
//! number of live clients (attached terminals, web chat streams). It is one
//! more [`Surface`] in the daemon's fan-out; clients subscribe and render.

use ka_agent_core::{ApprovalStatus, SessionStatus};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::status::StatusReport;
use crate::surface::{ApprovalCard, Surface};

/// Bounded backlog per subscriber; a slower client sees `Lagged(n)`.
const CAPACITY: usize = 1024;

/// One surface event on the wire (NDJSON for attach, SSE for the web).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum SurfaceEvent {
    /// First event on an attach connection (sent only by attach servers).
    Hello {
        /// Boxed: the report dwarfs every other variant (the wire format is
        /// unchanged — serde serializes the box transparently).
        report: Box<StatusReport>,
    },
    /// A router reply for one client; never broadcast.
    Reply {
        text: String,
    },
    Inbound {
        source: String,
        text: String,
    },
    TurnStarted,
    Delta {
        text: String,
    },
    Note {
        text: String,
    },
    Notice {
        text: String,
    },
    Approval {
        id: String,
        question: String,
        detail: Option<String>,
        expires_at_ms: i64,
    },
    ApprovalSettled {
        id: String,
        status: String,
    },
    TurnFinished {
        stop: String,
        cost_usd: f64,
        tokens: u64,
    },
    Status {
        status: String,
    },
    Error {
        class: String,
        message: String,
    },
}

pub struct EventBus {
    tx: broadcast::Sender<SurfaceEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SurfaceEvent> {
        self.tx.subscribe()
    }

    fn send(&self, ev: SurfaceEvent) {
        // No subscriber is not an error: nobody is watching right now.
        let _ = self.tx.send(ev);
    }
}

impl Surface for EventBus {
    fn on_turn_started(&self) {
        self.send(SurfaceEvent::TurnStarted);
    }

    fn on_delta_text(&self, text: &str) {
        self.send(SurfaceEvent::Delta {
            text: text.to_string(),
        });
    }

    fn on_note(&self, text: &str) {
        self.send(SurfaceEvent::Note {
            text: text.to_string(),
        });
    }

    fn on_notice(&self, text: &str) {
        self.send(SurfaceEvent::Notice {
            text: text.to_string(),
        });
    }

    fn on_inbound(&self, source: &str, text: &str) {
        self.send(SurfaceEvent::Inbound {
            source: source.to_string(),
            text: text.to_string(),
        });
    }

    fn on_approval(&self, card: &ApprovalCard) {
        self.send(SurfaceEvent::Approval {
            id: card.approval_id.clone(),
            question: card.question.clone(),
            detail: card.detail.clone(),
            expires_at_ms: card.expires_at_ms,
        });
    }

    fn on_approval_settled(&self, approval_id: &str, status: ApprovalStatus) {
        self.send(SurfaceEvent::ApprovalSettled {
            id: approval_id.to_string(),
            status: format!("{status:?}"),
        });
    }

    fn on_turn_finished(&self, stop: &str, cost_usd: f64, tokens: u64) {
        self.send(SurfaceEvent::TurnFinished {
            stop: stop.to_string(),
            cost_usd,
            tokens,
        });
    }

    fn on_session_status(&self, status: SessionStatus) {
        self.send(SurfaceEvent::Status {
            status: format!("{status:?}"),
        });
    }

    fn on_error(&self, class: &str, message: &str) {
        self.send(SurfaceEvent::Error {
            class: class.to_string(),
            message: message.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn events_serialize_with_a_snake_case_tag() {
        let v = serde_json::to_value(SurfaceEvent::TurnStarted).unwrap();
        assert_eq!(v, serde_json::json!({"ev": "turn_started"}));
        let v = serde_json::to_value(SurfaceEvent::Delta { text: "hi".into() }).unwrap();
        assert_eq!(v, serde_json::json!({"ev": "delta", "text": "hi"}));
    }

    #[test]
    fn subscribers_receive_surface_calls() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.on_inbound("telegram", "hello");
        bus.on_notice("⏰ ping");
        bus.on_final_chunk(1, "ignored: deltas already carried it");
        assert_eq!(
            rx.try_recv().unwrap(),
            SurfaceEvent::Inbound {
                source: "telegram".into(),
                text: "hello".into()
            }
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            SurfaceEvent::Notice {
                text: "⏰ ping".into()
            }
        );
        assert!(rx.try_recv().is_err());
    }
}
