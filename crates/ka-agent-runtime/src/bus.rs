//! The in-process event bus: every runtime envelope, broadcast to any
//! number of live clients (attached terminals, web chat streams). It is one
//! more [`Surface`] in the daemon's fan-out; clients subscribe and render.
//!
//! A ring of the last [`RING`] envelopes backs [`EventBus::subscribe_since`]:
//! a reconnecting client that names the last seq it saw gets exactly the
//! gap replayed (older gaps collapse into one `(missed events)` notice).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::event::{AgentEvent, Envelope};
use crate::surface::Surface;

/// Bounded backlog per subscriber; a slower client sees `Lagged(n)`.
const CAPACITY: usize = 1024;

/// How many recent envelopes a reconnect can replay.
const RING: usize = 256;

/// Seq for synthesized envelopes (`(missed events)`): every real envelope
/// is ≥ 1, so 0 is free.
const SYNTH_SEQ: u64 = 0;

pub struct EventBus {
    tx: broadcast::Sender<Arc<Envelope>>,
    ring: Mutex<VecDeque<Arc<Envelope>>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self {
            tx,
            ring: Mutex::new(VecDeque::with_capacity(RING)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Envelope>> {
        self.tx.subscribe()
    }

    /// Subscribe plus a replay of everything after `seq`: the ring entries
    /// with a newer seq, oldest first, and — when `seq` predates the ring —
    /// one leading `(missed events)` envelope. Subscribe happens before the
    /// snapshot so nothing between the two is lost.
    pub fn subscribe_since(
        &self,
        seq: u64,
    ) -> (broadcast::Receiver<Arc<Envelope>>, Vec<Arc<Envelope>>) {
        let rx = self.tx.subscribe();
        let mut replay = Vec::new();
        {
            let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(oldest) = ring.front()
                && seq + 1 < oldest.seq
            {
                replay.push(Arc::new(Envelope::new(
                    SYNTH_SEQ,
                    AgentEvent::Notice {
                        text: "(missed events)".into(),
                    },
                )));
            }
            replay.extend(ring.iter().filter(|e| e.seq > seq).cloned());
        }
        (rx, replay)
    }

    fn send(&self, env: Arc<Envelope>) {
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        if ring.len() == RING {
            ring.pop_front();
        }
        ring.push_back(Arc::clone(&env));
        drop(ring);
        // No subscriber is not an error: nobody is watching right now.
        let _ = self.tx.send(env);
    }
}

impl Surface for EventBus {
    fn emit(&self, env: &Envelope) {
        self.send(Arc::new(env.clone()));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn notice(text: &str, seq: u64) -> Envelope {
        Envelope::new(seq, AgentEvent::Notice { text: text.into() })
    }

    fn emit_n(bus: &EventBus, n: u64) {
        for i in 1..=n {
            bus.emit(&notice(&format!("e{i}"), i));
        }
    }

    #[test]
    fn subscribers_receive_emitted_envelopes_in_order() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.emit(&notice("hello", 1));
        bus.emit(&notice("ping", 2));
        assert_eq!(
            rx.try_recv().unwrap().ev,
            AgentEvent::Notice {
                text: "hello".into()
            }
        );
        assert_eq!(rx.try_recv().unwrap().seq, 2);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn subscribe_since_replays_exactly_the_gap() {
        let bus = EventBus::new();
        emit_n(&bus, 5);
        let (mut rx, replay) = bus.subscribe_since(2);
        let seqs: Vec<u64> = replay.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![3, 4, 5]);
        // The live stream starts after the replay, with no overlap.
        bus.emit(&notice("e6", 6));
        assert_eq!(rx.try_recv().unwrap().seq, 6);
    }

    #[test]
    fn subscribe_since_zero_replays_the_whole_ring() {
        let bus = EventBus::new();
        emit_n(&bus, 3);
        let (_, replay) = bus.subscribe_since(0);
        let seqs: Vec<u64> = replay.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn a_seq_older_than_the_ring_gets_a_missed_notice() {
        let bus = EventBus::new();
        emit_n(&bus, 300); // the ring holds 45..=300
        let (_, replay) = bus.subscribe_since(10);
        assert_eq!(replay.len(), RING + 1, "notice + the whole ring");
        assert_eq!(replay[0].seq, SYNTH_SEQ);
        assert_eq!(
            replay[0].ev,
            AgentEvent::Notice {
                text: "(missed events)".into()
            }
        );
        assert_eq!(replay[1].seq, 45);
        assert_eq!(replay.last().unwrap().seq, 300);
    }

    #[test]
    fn ring_boundary_plays_back_without_a_notice() {
        let bus = EventBus::new();
        emit_n(&bus, 300); // the ring holds 45..=300
        let (_, replay) = bus.subscribe_since(44);
        assert_eq!(replay.len(), RING);
        assert_eq!(replay[0].seq, 45);
        assert!(replay.iter().all(|e| e.seq != SYNTH_SEQ));
    }
}
