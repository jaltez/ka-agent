//! Fan-out surface: runtime outbound goes to every enabled channel, so an
//! operator working across Telegram and Matrix sees notes, answers and
//! approval cards wherever they are (Phase 6). Each channel still sends its
//! own direct replies through its transport.

use ka_agent_core::ApprovalStatus;
use ka_agent_runtime::surface::{ApprovalCard, Surface};
use std::sync::Arc;

pub struct MultiSurface {
    inner: Vec<Arc<dyn Surface>>,
}

impl MultiSurface {
    /// At least one surface is required; pass a `NullSurface` when no
    /// channel is enabled so a channel-less daemon stays observable.
    pub fn new(inner: Vec<Arc<dyn Surface>>) -> Self {
        Self { inner }
    }
}

impl Surface for MultiSurface {
    fn on_turn_started(&self) {
        for s in &self.inner {
            s.on_turn_started();
        }
    }

    fn on_delta_text(&self, text: &str) {
        for s in &self.inner {
            s.on_delta_text(text);
        }
    }

    fn on_delta_thought(&self, text: &str) {
        for s in &self.inner {
            s.on_delta_thought(text);
        }
    }

    fn on_note(&self, text: &str) {
        for s in &self.inner {
            s.on_note(text);
        }
    }

    fn on_notice(&self, text: &str) {
        for s in &self.inner {
            s.on_notice(text);
        }
    }

    fn on_inbound(&self, source: &str, text: &str) {
        for s in &self.inner {
            s.on_inbound(source, text);
        }
    }

    fn on_approval(&self, card: &ApprovalCard) {
        for s in &self.inner {
            s.on_approval(card);
        }
    }

    fn on_approval_settled(&self, approval_id: &str, status: ApprovalStatus) {
        for s in &self.inner {
            s.on_approval_settled(approval_id, status);
        }
    }

    fn on_final_chunk(&self, index: u32, text: &str) {
        for s in &self.inner {
            s.on_final_chunk(index, text);
        }
    }

    fn on_error(&self, class: &str, message: &str) {
        for s in &self.inner {
            s.on_error(class, message);
        }
    }

    fn on_turn_finished(&self, stop: &str, cost_usd: f64, tokens: u64) {
        for s in &self.inner {
            s.on_turn_finished(stop, cost_usd, tokens);
        }
    }

    fn on_session_status(&self, status: ka_agent_core::SessionStatus) {
        for s in &self.inner {
            s.on_session_status(status);
        }
    }
}
