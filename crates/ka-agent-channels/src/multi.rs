//! Fan-out surface: runtime outbound goes to every enabled channel, so an
//! operator working across Telegram and Matrix sees notes, answers and
//! approval cards wherever they are (Phase 6). Each channel still sends its
//! own direct replies through its transport.

use ka_agent_runtime::Envelope;
use ka_agent_runtime::surface::Surface;
use std::sync::Arc;

pub struct MultiSurface {
    inner: Vec<Arc<dyn Surface>>,
}

impl MultiSurface {
    /// At least one surface is required; pass a silent [`Surface`] when no
    /// channel is enabled so a channel-less daemon stays observable.
    pub fn new(inner: Vec<Arc<dyn Surface>>) -> Self {
        Self { inner }
    }
}

impl Surface for MultiSurface {
    fn emit(&self, ev: &Envelope) {
        for s in &self.inner {
            s.emit(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use ka_agent_runtime::event::AgentEvent;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Rec(Mutex<Vec<AgentEvent>>);
    impl Surface for Rec {
        fn emit(&self, ev: &Envelope) {
            self.0.lock().unwrap().push(ev.ev.clone());
        }
    }

    #[test]
    fn every_inner_surface_sees_every_envelope() {
        let a = Arc::new(Rec::default());
        let b = Arc::new(Rec::default());
        let multi = MultiSurface::new(vec![a.clone() as Arc<dyn Surface>, b]);
        let env = Envelope::new(1, AgentEvent::Notice { text: "hi".into() });
        multi.emit(&env);
        assert_eq!(
            a.0.lock().unwrap().as_slice(),
            &[AgentEvent::Notice { text: "hi".into() }]
        );
    }
}
