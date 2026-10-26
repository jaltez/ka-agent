//! Approval commands: the pending list and the y/n decisions.

use super::{CmdFuture, Ctx, Outcome};
use ka_agent_core::ids::{id_matches, short_id};
use ka_agent_runtime::Runtime;

pub(super) fn run_approvals<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        match ctx.runtime.pending_approval_cards() {
            Ok(cards) if cards.is_empty() => Outcome::Reply("no pending approvals".into()),
            Ok(cards) => Outcome::Reply(
                cards
                    .iter()
                    .map(|c| {
                        let question: String = c.question.lines().next().unwrap_or("").into();
                        format!(
                            "[{}] {} — expires in {}",
                            short_id(&c.approval_id),
                            question,
                            super::fmt_interval(c.expires_at_ms - ka_agent_core::now_ms())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Err(e) => Outcome::Reply(format!("! [approvals] {e}")),
        }
    })
}

pub(super) fn run_y<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move { decide(ctx.runtime, ctx.source, true, ctx.args).await })
}

pub(super) fn run_n<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move { decide(ctx.runtime, ctx.source, false, ctx.args).await })
}

/// `/y [id] [session]` / `/n [id]`: the shared decision flow. `allow` picks
/// the `ApprovalDecision`.
pub(super) async fn decide(runtime: &Runtime, source: &str, allow: bool, args: &str) -> Outcome {
    use ka_agent_core::ApprovalDecision;
    let decision = if allow {
        ApprovalDecision::Allow
    } else {
        ApprovalDecision::Deny
    };
    let mut prefix = args;
    // `/y [id] session`: allow this call and identical ones until
    // /new — terminal and web only (a phone grant would outlive the
    // glance it was given with).
    let session = allow && (prefix == "session" || prefix.ends_with(" session"));
    if session {
        if source.starts_with("telegram:") || source.starts_with("matrix:") {
            return Outcome::Reply(
                "allow-for-session is only offered in the terminal and the web panel".into(),
            );
        }
        prefix = prefix.trim_end_matches("session").trim();
        let cards = match runtime.pending_approval_cards() {
            Ok(c) => c,
            Err(e) => return Outcome::Reply(format!("! [approval] {e}")),
        };
        let pick: Vec<_> = if prefix.is_empty() {
            cards
                .iter()
                .max_by_key(|c| c.expires_at_ms)
                .into_iter()
                .collect()
        } else {
            cards
                .iter()
                .filter(|c| id_matches(&c.approval_id, prefix))
                .collect()
        };
        return match pick.as_slice() {
            [one] => match runtime.decide_for_session(&one.approval_id).await {
                Ok(()) => {
                    Outcome::Reply("allowed — identical requests are allowed until /new".into())
                }
                Err(e) => Outcome::Reply(format!("! [approval] {e}")),
            },
            [] => Outcome::Reply("no pending approval".into()),
            _ => Outcome::Reply("ambiguous approval id — /approvals lists them".into()),
        };
    }
    if prefix.is_empty() {
        match runtime.decide_latest(decision).await {
            Ok(true) => Outcome::Reply("decided".into()),
            Ok(false) => Outcome::Reply("no pending approval".into()),
            Err(e) => Outcome::Reply(format!("! [approval] {e}")),
        }
    } else {
        let cards = match runtime.pending_approval_cards() {
            Ok(c) => c,
            Err(e) => return Outcome::Reply(format!("! [approval] {e}")),
        };
        let matches: Vec<_> = cards
            .iter()
            .filter(|c| id_matches(&c.approval_id, prefix))
            .collect();
        match matches.as_slice() {
            [one] => match runtime.decide(&one.approval_id, decision).await {
                Ok(()) => Outcome::Reply("decided".into()),
                Err(e) => Outcome::Reply(format!("! [approval] {e}")),
            },
            [] => Outcome::Reply(format!("no pending approval matches {prefix}")),
            _ => Outcome::Reply("ambiguous approval id — /approvals lists them".into()),
        }
    }
}
