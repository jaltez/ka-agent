//! Session commands the engine answers between turns — `/diff`
//! (`session_diff`), `/context` (`context_breakdown`) and `/btw`
//! (`side_question`) — and the idle recap. None of them is a prompt and
//! none touches the strand's context: the engine answers each with one
//! typed event and then `idle`, so the single-writer rule holds
//! untouched (`docs/contract/ka-port.md` §4). The reply is an event
//! (`Notice`, plus a `File` for the whole diff), not a router reply: the
//! router has answered long before the engine does.

use super::*;
use ka_agent_core::Principal;
use ka_agent_core::ids::short_id;
use std::path::{Path, PathBuf};

/// A session command the engine answers between turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionOp {
    Diff,
    Context,
    Btw,
}

impl SessionOp {
    /// What the engine needs before it can answer: the refusal when no
    /// worker is live. Each names what it measures and where that lives.
    fn no_worker(self) -> &'static str {
        match self {
            // The journal `/diff` reads is the engine's, on disk: a worker
            // that restarts picks it up whole, so nothing is lost by waiting.
            SessionOp::Diff => {
                "no live session — the engine is off. The session's change \
                 journal is on disk, so nothing is lost: any message starts the \
                 worker, then /diff again"
            }
            SessionOp::Context => {
                "no live session — the context meter lives in the engine: any \
                 message starts the worker, then /context again"
            }
            SessionOp::Btw => {
                "no live session — /btw asks the engine's fast model: any \
                 message starts the worker, then /btw again"
            }
        }
    }
}

/// A session command in flight on the worker.
#[derive(Debug)]
pub(super) struct PendingOp {
    pub op: SessionOp,
    /// The surface that asked (`console`, `telegram:<id>`, …): the reply
    /// is stamped with it, so it reaches the asker — not whoever ran the
    /// last turn.
    pub source: String,
}

/// The store meta key of a session's latest idle recap.
fn recap_meta_key(session: &str) -> String {
    format!("session.recap.{session}")
}

impl Runtime {
    /// `/diff`: the session's cumulative file diff. The engine answers
    /// with `SessionDiff` — then `idle` — and the event pump replies on
    /// `source`, attaching the whole diff when the summary parks it.
    pub async fn session_diff(&self, who: &Principal, source: &str) -> Result<(), Error> {
        self.send_session_op(who, source, SessionOp::Diff, "").await
    }

    /// `/context`: the context-usage breakdown. The engine answers with
    /// `ContextBreakdown` — then `idle` — and the event pump renders it
    /// for `source`.
    pub async fn context_breakdown(&self, who: &Principal, source: &str) -> Result<(), Error> {
        self.send_session_op(who, source, SessionOp::Context, "")
            .await
    }

    /// `/btw <question>`: the engine's fast role answers from the
    /// question alone (no tools, the transcript and the context
    /// untouched). The engine answers with `SideAnswer` — then `idle`.
    pub async fn side_question(
        &self,
        who: &Principal,
        question: &str,
        source: &str,
    ) -> Result<(), Error> {
        self.send_session_op(who, source, SessionOp::Btw, question)
            .await
    }

    /// Send one session command on `who`'s live worker. The engine takes
    /// these only between turns — mid-cycle it swallows them unheard — so
    /// a session that is not idle refuses with the reason; the command is
    /// claimed under the same lock as that check, and before it leaves, so
    /// an answer can never arrive ahead of the command that asked for it.
    async fn send_session_op(
        &self,
        who: &Principal,
        source: &str,
        op: SessionOp,
        question: &str,
    ) -> Result<(), Error> {
        let st = self.ensure_user(who)?;
        let port = {
            let mut g = st.lock();
            match g.session.status {
                SessionStatus::Running
                | SessionStatus::AwaitingApproval
                | SessionStatus::Stopping => {
                    return Err(Error::Refused(
                        "the turn is still running — ask again when it ends".into(),
                    ));
                }
                SessionStatus::Degraded => {
                    return Err(Error::Refused(
                        "the worker keeps crashing — /new starts a fresh session".into(),
                    ));
                }
                _ => {}
            }
            let port = g
                .live
                .as_ref()
                .map(|l| Arc::clone(&l.port))
                .ok_or_else(|| Error::Refused(op.no_worker().to_string()))?;
            if g.session.status == SessionStatus::Paused {
                return Err(Error::Refused(
                    "the session is paused — /resume first".into(),
                ));
            }
            if let Some(live) = g.live.as_mut() {
                live.op = Some(PendingOp {
                    op,
                    source: source.to_string(),
                });
            }
            port
        };
        let sent = match op {
            SessionOp::Diff => port.session_diff().await,
            SessionOp::Context => port.context_breakdown().await,
            SessionOp::Btw => port.side_question(question.to_string()).await,
        };
        if let Err(e) = sent {
            if let Some(live) = st.lock().live.as_mut() {
                live.op = None;
            }
            return Err(Error::Port(e));
        }
        Ok(())
    }
}

/// `/context`'s line: the used total against the window, then every
/// component — `context 70.0k / 200k (35%) — system 9.5k · user 40.1k ·
/// assistant 12.3k · tools 8.0k`. The engine's `hint:` rows are advice
/// on what could be trimmed, not usage: they stay out of the total and
/// follow on a line of their own. A window of 0 is unknown.
pub fn render_context_breakdown(parts: &[(String, u64)], window: u64) -> String {
    let mut usage: Vec<(&str, u64)> = Vec::with_capacity(parts.len());
    let mut hints: Vec<(&str, u64)> = Vec::new();
    for (name, tokens) in parts {
        if name.starts_with("hint:") {
            hints.push((name.as_str(), *tokens));
        } else {
            usage.push((name.as_str(), *tokens));
        }
    }
    let used = usage
        .iter()
        .fold(0u64, |sum, (_, tokens)| sum.saturating_add(*tokens));
    let head = match window {
        0 => format!("context {} / window unknown", k_tokens(used)),
        window => {
            // Rounded, like a meter a human reads: 69.9k of 200k is 35%.
            let pct = used
                .checked_mul(100)
                .and_then(|v| v.checked_add(window / 2))
                .and_then(|v| v.checked_div(window))
                .unwrap_or(0);
            format!("context {} / {} ({pct}%)", k_tokens(used), k_window(window))
        }
    };
    let mut out = head;
    if !usage.is_empty() {
        let parts: Vec<String> = usage
            .iter()
            .map(|(name, tokens)| format!("{name} {}", k_tokens(*tokens)))
            .collect();
        out.push_str(" — ");
        out.push_str(&parts.join(" · "));
    }
    if !hints.is_empty() {
        let hints: Vec<String> = hints
            .iter()
            .map(|(name, tokens)| {
                format!(
                    "{} ≈ {}",
                    name.strip_prefix("hint:").unwrap_or(name),
                    k_tokens(*tokens)
                )
            })
            .collect();
        out.push_str("\ntrim: ");
        out.push_str(&hints.join(" · "));
    }
    out
}

/// A token count as the breakdown shows it: `9.5k`, `70.0k`; under a
/// thousand, the number itself.
fn k_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// The window's token count: a whole thousand reads `200k`.
fn k_window(n: u64) -> String {
    if n >= 1000 && n.is_multiple_of(1000) {
        format!("{}k", n / 1000)
    } else {
        k_tokens(n)
    }
}

/// The `spill://<id>` pointer of a `/diff` summary — on its first line,
/// when the full diff was parked on the spill store.
fn diff_pointer(summary: &str) -> Option<&str> {
    summary
        .lines()
        .next()?
        .split_whitespace()
        .find(|word| word.starts_with("spill://"))
}

/// Copy the full diff where every surface can hand it over: the
/// session's attachments, `<workspace>/attachments/` — the directory the
/// web panel serves and `@attachments/<name>` references (a private
/// conversation's is destroyed with it). The spill file itself is the
/// engine's, swept after a week. One file per session, rewritten by each
/// `/diff`: the diff is the whole session's, so the newest is the only
/// one that matters.
fn park_diff(
    spill: &Path,
    workspace: &Path,
    private_short: Option<&str>,
    session_short: &str,
) -> Result<(PathBuf, String), String> {
    let dir = match private_short {
        Some(short) => workspace
            .join("attachments")
            .join(format!(".private-{short}")),
        None => workspace.join("attachments"),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = format!("session-diff-{session_short}.diff");
    let path = dir.join(&name);
    std::fs::copy(spill, &path).map_err(|e| format!("{}: {e}", spill.display()))?;
    Ok((path, name))
}

/// Answer a `/diff` on the event pump: the summary is the reply; when it
/// parks the whole diff on a spill file, that file is attached through
/// the file affordance, so Telegram, the web panel and the terminal hand
/// over all of it.
pub(super) async fn reply_diff(sh: &Arc<Shared>, st: &UserHandle, source: &str, summary: String) {
    sh.emit_from(
        AgentEvent::Notice {
            text: summary.clone(),
        },
        Some(source),
    );
    let Some(pointer) = diff_pointer(&summary) else {
        return;
    };
    let (state_dir, workspace, private_short, session_short) = {
        let g = st.lock();
        let session = &g.session;
        let short = short_id(session.id.as_str()).to_string();
        (
            g.live.as_ref().map(|l| l.state_dir.clone()),
            session.workspace.clone(),
            session.private.then_some(short.clone()),
            short,
        )
    };
    let Some(spill) = state_dir
        .as_deref()
        .and_then(|dir| ka_agent_port::spill_path(dir, pointer))
    else {
        return;
    };
    let parked = tokio::task::spawn_blocking(move || {
        park_diff(&spill, &workspace, private_short.as_deref(), &session_short)
    })
    .await
    .unwrap_or_else(|e| Err(format!("copy task: {e}")));
    match parked {
        Ok((path, name)) => sh.emit_from(
            AgentEvent::File {
                path: path.to_string_lossy().into_owned(),
                name,
                caption: "the session's whole diff".into(),
            },
            Some(source),
        ),
        Err(e) => sh.emit_from(
            AgentEvent::Notice {
                text: format!("! [diff] the whole diff could not be attached ({e})"),
            },
            Some(source),
        ),
    }
}

/// Take the pending op's source when the arriving answer matches it:
/// the reply belongs to the surface that asked, not to the last turn.
pub(super) fn take_source(st: &UserHandle, op: SessionOp) -> Option<String> {
    let mut g = st.lock();
    let live = g.live.as_mut()?;
    if live.op.as_ref().is_some_and(|p| p.op == op) {
        live.op.take().map(|p| p.source)
    } else {
        None
    }
}

/// Clear any pending op (a turn started, the worker went away): nothing
/// will answer it any more.
pub(super) fn clear_op(st: &UserHandle) {
    if let Some(live) = st.lock().live.as_mut() {
        live.op = None;
    }
}

/// An idle recap: kept on the session for `/status`, and shown where
/// notes are. A private conversation keeps nothing of what it said.
pub(super) fn record_recap(sh: &Arc<Shared>, st: &UserHandle, text: String) {
    let (sid, private) = {
        let g = st.lock();
        (g.session.id.clone(), g.session.private)
    };
    if private {
        return;
    }
    let recap = crate::status::LastRecap {
        text: text.clone(),
        at_ms: ka_agent_core::now_ms(),
    };
    let saved = serde_json::to_string(&recap)
        .map_err(|e| e.to_string())
        .and_then(|value| {
            sh.store
                .meta_set(&recap_meta_key(sid.as_str()), &value)
                .map_err(|e| e.to_string())
        });
    if let Err(e) = saved {
        sh.emit(AgentEvent::Error {
            class: "internal".into(),
            retryable: retryable_class("internal"),
            message: format!("recap persist: {e}"),
        });
    }
    sh.emit(AgentEvent::Note { text });
}

/// The session's latest idle recap, when it kept one.
pub(super) fn last_recap(
    store: &Store,
    sid: &SessionId,
) -> Result<Option<crate::status::LastRecap>, Error> {
    Ok(store
        .meta_get(&recap_meta_key(sid.as_str()))?
        .and_then(|value| serde_json::from_str(&value).ok()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn parts(rows: &[(&str, u64)]) -> Vec<(String, u64)> {
        rows.iter().map(|(n, t)| (n.to_string(), *t)).collect()
    }

    #[test]
    fn a_recap_round_trips_through_the_session_meta() {
        let store = Store::open_in_memory().unwrap();
        let sid = ka_agent_core::SessionId::generate();
        let recap = crate::status::LastRecap {
            text: "mid-refactor on the parser".into(),
            at_ms: 42,
        };
        store
            .meta_set(
                &recap_meta_key(sid.as_str()),
                &serde_json::to_string(&recap).unwrap(),
            )
            .unwrap();
        assert_eq!(last_recap(&store, &sid).unwrap(), Some(recap));
        // None is no error: a session that never had a recap.
        let other = ka_agent_core::SessionId::generate();
        assert_eq!(last_recap(&store, &other).unwrap(), None);
    }

    #[test]
    fn context_renders_total_components_and_window() {
        let text = render_context_breakdown(
            &parts(&[
                ("system", 9_500),
                ("user", 40_100),
                ("assistant", 12_300),
                ("tools", 8_000),
            ]),
            200_000,
        );
        assert_eq!(
            text,
            "context 69.9k / 200k (35%) — system 9.5k · user 40.1k · assistant 12.3k · tools 8.0k"
        );
        // An unknown window: no percentage, and it says so.
        let text = render_context_breakdown(&parts(&[("system", 999), ("user", 1_234)]), 0);
        assert_eq!(
            text,
            "context 2.2k / window unknown — system 999 · user 1.2k"
        );
        // A window that is not a whole thousand keeps its decimals.
        assert!(
            render_context_breakdown(&parts(&[]), 199_500).starts_with("context 0 / 199.5k (0%)")
        );
    }

    #[test]
    fn context_keeps_trim_hints_out_of_the_total() {
        let text = render_context_breakdown(
            &parts(&[
                ("system", 9_000),
                ("hint:thinking 12.3 KB", 3_072),
                ("user", 1_000),
                ("hint:outputs ~900 tok", 900),
            ]),
            100_000,
        );
        assert_eq!(
            text,
            "context 10.0k / 100k (10%) — system 9.0k · user 1.0k\n\
             trim: thinking 12.3 KB ≈ 3.1k · outputs ~900 tok ≈ 900"
        );
    }

    #[test]
    fn a_diff_pointer_rides_the_summary_line_only() {
        let summary = "3 file(s), 42 +/- lines — full: spill://1730000000000-7\n\n+hi";
        assert_eq!(diff_pointer(summary), Some("spill://1730000000000-7"));
        assert_eq!(diff_pointer("no changes"), None);
        // An excerpt that quotes a pointer is not one.
        assert_eq!(diff_pointer("no changes\n\nspill://in-the-diff"), None);
    }

    #[test]
    fn the_whole_diff_lands_in_the_session_attachments() {
        let root = std::env::temp_dir().join(format!("ka-diff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let spill = root.join("state/ka/spills/1730-7");
        std::fs::create_dir_all(spill.parent().unwrap()).unwrap();
        std::fs::write(&spill, "+the whole diff\n").unwrap();
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        let (path, name) = park_diff(&spill, &ws, None, "ab12cd34").unwrap();
        assert_eq!(name, "session-diff-ab12cd34.diff");
        assert_eq!(path, ws.join("attachments").join(&name));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "+the whole diff\n");

        // A private conversation keeps its copy where its other remnants
        // are, the directory the runtime destroys when it ends.
        let (path, name) = park_diff(&spill, &ws, Some("ab12cd34"), "ab12cd34").unwrap();
        assert_eq!(path, ws.join("attachments/.private-ab12cd34").join(&name));

        // A spill file that is gone is named, not invented.
        let err = park_diff(&root.join("state/ka/spills/gone"), &ws, None, "x").unwrap_err();
        assert!(err.contains("gone"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
