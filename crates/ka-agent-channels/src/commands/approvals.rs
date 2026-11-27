//! Approval commands: the pending list and the y/n decisions.

use super::{CmdFuture, Ctx, Outcome};
use ka_agent_core::approval::{NOT_A_PERMISSION_ASK, is_permission_ask};
use ka_agent_core::ids::{id_matches, short_id};
use ka_agent_runtime::{ApprovalCard, Runtime};

pub(super) fn run_approvals<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        match ctx.runtime.pending_approval_cards(&ctx.principal) {
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
    Box::pin(async move { decide(ctx.runtime, ctx.source, true, ctx.args, &ctx.principal).await })
}

pub(super) fn run_n<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move { decide(ctx.runtime, ctx.source, false, ctx.args, &ctx.principal).await })
}

/// `always` and `session` are grants, and a grant only exists on a
/// permission ask (`allow`, `always`, `deny`). On any other ask — the
/// engine's `continue`/`stop` session guards, an MCP elicitation — the
/// command refuses before anything is stored or answered. The runtime
/// refuses the same asks too (`decide_always`, `decide_for_session`) for
/// every surface; this keeps the reply a plain line.
fn grant_refusal(card: &ApprovalCard) -> Option<Outcome> {
    (!is_permission_ask(&card.options)).then(|| Outcome::Reply(NOT_A_PERMISSION_ASK.into()))
}

/// The one card `/y [id] always|session` acts on: the latest-expiring card
/// when no id is given, else the id's match. `Err` is the reply that ends
/// the command instead.
fn grant_target(cards: &[ApprovalCard], prefix: &str) -> Result<ApprovalCard, Outcome> {
    let pick: Vec<&ApprovalCard> = if prefix.is_empty() {
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
    match pick.as_slice() {
        [one] => match grant_refusal(one) {
            Some(reply) => Err(reply),
            None => Ok((*one).clone()),
        },
        [] => Err(Outcome::Reply("no pending approval".into())),
        _ => Err(Outcome::Reply(
            "ambiguous approval id — /approvals lists them".into(),
        )),
    }
}

/// `/y [id] [session]` / `/n [id]`: the shared decision flow. `allow` picks
/// the `ApprovalDecision`.
pub(super) async fn decide(
    runtime: &Runtime,
    source: &str,
    allow: bool,
    args: &str,
    principal: &ka_agent_core::Principal,
) -> Outcome {
    use ka_agent_core::ApprovalDecision;
    let decision = if allow {
        ApprovalDecision::Allow
    } else {
        ApprovalDecision::Deny
    };
    let mut prefix = args;
    // `/y [id] always` (v4 B7/B8): settle Allow and persist a durable
    // rule. Terminal and web only, exactly like `session` — a durable
    // rule minted from a phone would outlive the glance many times over.
    let always = allow && (prefix == "always" || prefix.ends_with(" always"));
    if always {
        if source.starts_with("telegram:") || source.starts_with("matrix:") {
            return Outcome::Reply(
                "always-rules are only offered in the terminal and the web panel".into(),
            );
        }
        prefix = prefix.trim_end_matches("always").trim();
        let cards = match runtime.pending_approval_cards(principal) {
            Ok(c) => c,
            Err(e) => return Outcome::Reply(format!("! [approval] {e}")),
        };
        let one = match grant_target(&cards, prefix) {
            Ok(one) => one,
            Err(reply) => return reply,
        };
        return match runtime.decide_always(&one.approval_id, source).await {
            Ok(text) => Outcome::Reply(text),
            Err(e) => Outcome::Reply(format!("! [approval] {e}")),
        };
    }
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
        let cards = match runtime.pending_approval_cards(principal) {
            Ok(c) => c,
            Err(e) => return Outcome::Reply(format!("! [approval] {e}")),
        };
        let one = match grant_target(&cards, prefix) {
            Ok(one) => one,
            Err(reply) => return reply,
        };
        return match runtime.decide_for_session(&one.approval_id, source).await {
            Ok(()) => Outcome::Reply("allowed — identical requests are allowed until /new".into()),
            Err(e) => Outcome::Reply(format!("! [approval] {e}")),
        };
    }
    if prefix.is_empty() {
        match runtime.decide_latest(decision, source).await {
            Ok(true) => Outcome::Reply("decided".into()),
            Ok(false) => Outcome::Reply("no pending approval".into()),
            Err(e) => Outcome::Reply(format!("! [approval] {e}")),
        }
    } else {
        let cards = match runtime.pending_approval_cards(principal) {
            Ok(c) => c,
            Err(e) => return Outcome::Reply(format!("! [approval] {e}")),
        };
        let matches: Vec<_> = cards
            .iter()
            .filter(|c| id_matches(&c.approval_id, prefix))
            .collect();
        match matches.as_slice() {
            [one] => match runtime.decide(&one.approval_id, decision, source).await {
                Ok(()) => Outcome::Reply("decided".into()),
                Err(e) => Outcome::Reply(format!("! [approval] {e}")),
            },
            [] => Outcome::Reply(format!("no pending approval matches {prefix}")),
            _ => Outcome::Reply("ambiguous approval id — /approvals lists them".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn card(options: &[&str]) -> ApprovalCard {
        ApprovalCard {
            approval_id: "ap-1".into(),
            question: "Which one?".into(),
            detail: None,
            options: options.iter().map(|o| o.to_string()).collect(),
            expires_at_ms: 0,
        }
    }

    #[test]
    fn grants_are_refused_on_every_ask_but_a_permission_ask() {
        assert!(grant_refusal(&card(&["allow", "always", "deny"])).is_none());
        for options in [
            &["a", "b", "c", "decline"][..],
            &["true", "false", "decline"][..],
            &["accept", "decline"][..],
            &["continue", "stop"][..],
        ] {
            match grant_refusal(&card(options)) {
                Some(Outcome::Reply(text)) => {
                    assert_eq!(text, NOT_A_PERMISSION_ASK, "{options:?}");
                }
                None => panic!("{options:?}: a grant must be refused here"),
                _ => panic!("{options:?}: a grant is refused with its reason"),
            }
        }
    }

    #[test]
    fn a_grant_never_lands_on_an_ask_with_its_own_options() {
        let mut permission = card(&["allow", "always", "deny"]);
        permission.approval_id = "ap-aaaa-1".into();
        permission.expires_at_ms = 10;
        let mut own = card(&["a", "b", "c", "decline"]);
        own.approval_id = "ap-bbbb-2".into();
        own.expires_at_ms = 20;
        let cards = [permission, own];
        // No id: the pick is the latest-expiring card — refused, with the
        // reason, before the runtime is asked for anything.
        match grant_target(&cards, "") {
            Err(Outcome::Reply(text)) => assert_eq!(text, NOT_A_PERMISSION_ASK),
            Err(_) => panic!("a refusal is always a reply"),
            Ok(one) => panic!(
                "a grant landed on {} with options {:?}",
                one.approval_id, one.options
            ),
        }
        // By id: the permission ask is the grant's target.
        assert!(matches!(
            grant_target(&cards, "ap-aaaa"),
            Ok(one) if one.approval_id == "ap-aaaa-1"
        ));
        // Nothing (unambiguous) to act on.
        for (cards, prefix) in [(&cards[..], "ap-zzzz"), (&cards[..0], "")] {
            assert!(
                matches!(
                    &grant_target(cards, prefix),
                    Err(Outcome::Reply(text)) if text == "no pending approval"
                ),
                "nothing to act on must say so ({prefix:?})"
            );
        }
    }
}
