//! Cross-machine invariant checks: the November 2026 postmortems turned
//! into asserts. The cycle boundary's state is spread over the session
//! row, the turn row, the in-memory `Live` mirror and the outbox — every
//! incident that week was an edge somebody forgot between two of those.
//! These predicates run at the boundary events only (never per-delta),
//! are opt-in for release builds via `KA_CHECK_INVARIANTS=1` (always on
//! in debug), and report as `internal` errors — the daemon keeps serving;
//! the violation lands in the journal and on the surfaces.

use std::sync::{Arc, LazyLock};

use ka_agent_core::{SessionStatus, TurnStatus};

use super::{Shared, UserHandle};
use crate::event::AgentEvent;

static ENABLED: LazyLock<bool> = LazyLock::new(|| {
    cfg!(debug_assertions)
        || std::env::var_os("KA_CHECK_INVARIANTS").is_some_and(|v| v != "0")
});

pub(super) fn enabled() -> bool {
    *ENABLED
}

/// The cross-machine facts one boundary event leaves behind. Only facts
/// that cannot skew during a concurrent settle are hard invariants — a
/// checker that cries wolf gets unplugged.
#[derive(Debug)]
pub(super) struct Snap {
    pub status: SessionStatus,
    pub asks: usize,
    pub live_port: bool,
    pub turn: Option<TurnStatus>,
    /// The claimed turn id has no store row.
    pub turn_row_missing: bool,
    /// `sent` outbox chunks of the current turn with empty text.
    pub sent_empty_chunks: usize,
    /// Memory status ≠ the persisted session row (transient-possible:
    /// reported, not asserted).
    pub status_skew: bool,
}

/// Pure predicates — unit-testable without a runtime.
pub(super) fn violations(s: &Snap) -> Vec<&'static str> {
    let mut out = Vec::new();
    if s.turn.is_some_and(|t| t.is_terminal()) && s.asks > 0 {
        out.push(
            "terminal turn with pending asks (orphan ask — the 2026-11-17 wedge)",
        );
    }
    if matches!(
        s.status,
        SessionStatus::Running | SessionStatus::AwaitingApproval | SessionStatus::Stopping
    ) && !s.live_port
    {
        out.push("busy session without a live worker");
    }
    if s.turn_row_missing {
        out.push("claimed turn without a store row");
    }
    if s.sent_empty_chunks > 0 {
        out.push("outbox chunk delivered empty (the 2026-11-16 silent family)");
    }
    if s.status_skew {
        out.push("memory/store session status skew (transient-possible)");
    }
    out
}

fn snap(sh: &Arc<Shared>, st: &UserHandle) -> Snap {
    let (status, asks, live_port, sid, turn_id) = {
        let g = st.lock();
        (
            g.session.status,
            g.live.as_ref().map_or(0, |l| l.pending_asks.len()),
            g.live.is_some(),
            g.session.id.clone(),
            g.live.as_ref().and_then(|l| l.current_turn.clone()),
        )
    };
    let turn = turn_id
        .as_ref()
        .and_then(|t| sh.store.get_turn(t).ok().flatten())
        .map(|t| t.status);
    let sent_empty_chunks = turn_id
        .as_ref()
        .map(|t| {
            sh.store
                .chunks_for_turn(t)
                .map(|cs| {
                    cs.iter()
                        .filter(|c| c.status == ka_agent_core::OutboxStatus::Sent)
                        .filter(|c| c.text.trim().is_empty())
                        .count()
                })
                .unwrap_or(0)
        })
        .unwrap_or(0);
    let store_status = sh.store.get_session(&sid).ok().map(|s| s.status);
    Snap {
        status,
        asks,
        live_port,
        turn,
        turn_row_missing: turn_id.is_some() && turn.is_none(),
        sent_empty_chunks,
        status_skew: store_status.is_some_and(|s| s != status),
    }
}

/// Gather the snapshot and report every violation. Called at the four
/// boundary events of the drive loop (`Ask`, `TurnFinished`, `Idle`,
/// `EngineClosed`) — never on deltas.
pub(super) fn check_and_report(sh: &Arc<Shared>, st: &UserHandle) {
    if !enabled() {
        return;
    }
    for v in violations(&snap(sh, st)) {
        eprintln!("invariant violated: {v}");
        sh.emit(AgentEvent::Error {
            class: "internal".into(),
            retryable: false,
            message: format!("invariant violated: {v}"),
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn snap(status: SessionStatus, asks: usize, turn: Option<TurnStatus>) -> Snap {
        Snap {
            status,
            asks,
            live_port: true,
            turn,
            turn_row_missing: false,
            sent_empty_chunks: 0,
            status_skew: false,
        }
    }

    #[test]
    fn the_2026_11_17_wedge_is_a_violation() {
        let s = snap(SessionStatus::AwaitingApproval, 1, Some(TurnStatus::Aborted));
        assert_eq!(
            violations(&s),
            vec!["terminal turn with pending asks (orphan ask — the 2026-11-17 wedge)"]
        );
    }

    #[test]
    fn a_healthy_boundary_is_clean() {
        // Terminal turn, asks swept, worker still attached, store in sync.
        let s = snap(SessionStatus::Idle, 0, Some(TurnStatus::Done));
        assert!(violations(&s).is_empty());
        // A live ask on a running turn is the machine working as designed.
        let s = snap(SessionStatus::AwaitingApproval, 1, Some(TurnStatus::Running));
        assert!(violations(&s).is_empty());
    }

    #[test]
    fn busy_without_worker_and_empty_delivery() {
        let mut s = snap(SessionStatus::Running, 0, Some(TurnStatus::Running));
        s.live_port = false;
        assert!(violations(&s).contains(&"busy session without a live worker"));
        let mut s = snap(SessionStatus::Idle, 0, Some(TurnStatus::Done));
        s.sent_empty_chunks = 1;
        assert!(violations(&s).contains(&"outbox chunk delivered empty (the 2026-11-16 silent family)"));
    }
}
