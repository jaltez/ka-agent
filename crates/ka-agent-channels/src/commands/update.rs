//! `/update`: release check and permission-gated self-update.
//!
//! The install path restarts the daemon, so it refuses while a turn is
//! in flight (a paused or degraded session has no live turn and may
//! update), runs one install at a time, and only proceeds when the
//! daemon can actually replace its own binary — the probe inside the
//! hook reports a system-wide or locked-down install with the manual
//! remedy instead of downloading anything.

use super::{CmdFuture, Ctx, Outcome};
use ka_agent_core::SessionStatus;

pub(super) fn run_update<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let check_only = match ctx.args.trim() {
            "" => false,
            "check" => true,
            _ => return Outcome::Reply("usage: /update [check]".into()),
        };
        let runtime = ctx.runtime;
        // An install restarts the daemon — never while a turn is in
        // flight. Paused/Degraded sessions have no live turn: updating
        // then is safe (and `/stop` could not clear them anyway). The
        // check-only form just reads release metadata and is never
        // gated.
        if !check_only
            && matches!(
                runtime.session_status_for(&ctx.principal),
                SessionStatus::Running | SessionStatus::AwaitingApproval | SessionStatus::Stopping
            )
        {
            return Outcome::Reply(
                "! [update] a turn is running — /stop it first (an update restarts the daemon)"
                    .into(),
            );
        }
        let Some(updater) = runtime.updater() else {
            return Outcome::Reply(
                "! [update] self-update is not available here — run `ka-agent update` on the machine"
                    .into(),
            );
        };
        if !check_only && !runtime.try_begin_update() {
            return Outcome::Reply("! [update] an update is already running".into());
        }
        // The hook blocks (release metadata, download, the image swap):
        // keep it off the async workers, exactly like the daemon's
        // periodic release check does.
        let reply = tokio::task::spawn_blocking(move || updater(check_only))
            .await
            .unwrap_or_else(|e| Err(format!("update task failed: {e}")));
        if !check_only {
            runtime.end_update();
        }
        Outcome::Reply(match reply {
            Ok(text) => text,
            Err(e) => format!("! [update] {e}"),
        })
    })
}
