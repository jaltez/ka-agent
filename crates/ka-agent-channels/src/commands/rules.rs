//! `/rules` (v4 B7): list and remove durable allow-rules. A user sees
//! only their own; the admin sees every subject's and may remove any.

use super::{CmdFuture, Ctx, Outcome};

pub(super) fn run_rules<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let args = ctx.args.trim();
        if let Some(id) = args.strip_prefix("rm ") {
            let id = id.trim();
            if id.is_empty() {
                return Outcome::Reply("usage: /rules rm <id>".into());
            }
            return match ctx
                .runtime
                .rule_remove(id, &ctx.principal, ctx.source)
                .await
            {
                Ok(true) => Outcome::Reply(format!("removed rule {id}")),
                Ok(false) => Outcome::Reply(format!("no rule of yours matches {id}")),
                Err(e) => Outcome::Reply(format!("! [rules] {e}")),
            };
        }
        if !args.is_empty() && args != "list" {
            return Outcome::Reply("usage: /rules · /rules rm <id>".into());
        }
        match ctx.runtime.rules_list(&ctx.principal) {
            Ok(rules) if rules.is_empty() => {
                Outcome::Reply("no rules — /y <id> always offers one on a pending approval".into())
            }
            Ok(rules) => {
                let mut out = String::from("durable rules:\n");
                for r in &rules {
                    out.push_str(&format!(
                        "{} · {} · {} · {}{}\n",
                        r.id,
                        r.shape.label(),
                        r.mode,
                        r.subject,
                        r.from_approval
                            .as_deref()
                            .map(|a| format!(" (from {a})"))
                            .unwrap_or_default()
                    ));
                }
                out.push_str("remove with /rules rm <id>");
                Outcome::Reply(out)
            }
            Err(e) => Outcome::Reply(format!("! [rules] {e}")),
        }
    })
}
