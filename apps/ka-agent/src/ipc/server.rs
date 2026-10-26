//! The admin protocol over the IPC stream: one-shot JSON requests
//! (`status`, `shutdown`, `reload`, `command`) and the long-lived
//! `attach` stream used by `ka-agent chat`.

use std::path::PathBuf;
use std::sync::Arc;

use ka_agent_core::DaemonConfig;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::{Envelope, EventBus, Runtime};

use super::IpcStream;

/// Everything an admin connection needs.
#[derive(Clone)]
pub(crate) struct AdminCtx {
    pub(crate) runtime: Runtime,
    pub(crate) store: Arc<ka_agent_store::Store>,
    pub(crate) bus: Arc<EventBus>,
    pub(crate) shutdown: tokio::sync::watch::Sender<bool>,
    /// The config file the host loaded (re-read by `reload`).
    pub(crate) config: PathBuf,
}

/// First request line ceiling: anything bigger is not one of ours, and an
/// unbounded read hands any local peer an unbounded buffer.
const MAX_REQUEST: u64 = 8 * 1024;
/// Per-line ceiling on an attach stream (a pasted message fits).
const MAX_ATTACH_LINE: u64 = 64 * 1024;

/// Read one `\n`-terminated line of at most `max` bytes. `Ok(None)` on EOF
/// or an over-long line (the caller closes the connection).
async fn read_bounded_line<R>(r: &mut R, max: u64) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut buf = Vec::new();
    let n = (&mut *r).take(max + 1).read_until(b'\n', &mut buf).await?;
    if n == 0 || (buf.len() as u64) > max || !buf.ends_with(b"\n") {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&buf).trim().to_string()))
}

pub(crate) async fn serve_admin(stream: IpcStream, ctx: AdminCtx) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let peer_ok = stream.peer_ok();
    let (r, mut w) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(r);
    let Some(line) = read_bounded_line(&mut reader, MAX_REQUEST).await? else {
        return Ok(());
    };
    if line.is_empty() {
        return Ok(());
    }
    let reply: serde_json::Value = if !peer_ok {
        serde_json::json!({"error": "forbidden"})
    } else {
        let req = serde_json::from_str::<serde_json::Value>(&line).ok();
        let op = req
            .as_ref()
            .and_then(|r| r.get("op"))
            .and_then(|o| o.as_str())
            .map(str::to_string);
        match (req, op.as_deref()) {
            (Some(_), Some("status")) => match ctx.runtime.status_report() {
                Ok(report) => serde_json::json!({"ok": true, "report": report}),
                Err(e) => serde_json::json!({"error": e.to_string()}),
            },
            // Graceful stop (CLI-only policy, §5): the process exits 0 and
            // the service manager brings it back — this is what
            // `ka-agent restart` drives.
            (Some(_), Some("shutdown")) => {
                let _ = ctx.shutdown.send(true);
                serde_json::json!({"ok": true, "shutting_down": true})
            }
            (Some(req), Some("reload")) => {
                let restart_worker = req
                    .get("restart_worker")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                match DaemonConfig::load(&ctx.config) {
                    Ok(cfg) => {
                        let report = ctx.runtime.reload_config(cfg, restart_worker).await;
                        serde_json::json!({"ok": true, "report": report})
                    }
                    Err(e) => serde_json::json!({"error": e.to_string()}),
                }
            }
            // One router command (`ka-agent approvals` and friends): the
            // same handling as typing it in the chat, reply returned.
            (Some(req), Some("command")) => {
                let text = req.get("text").and_then(|t| t.as_str()).unwrap_or("");
                match ka_agent_channels::commands::route(&ctx.runtime, "cli", text).await {
                    ka_agent_channels::commands::Outcome::Reply(reply) => {
                        serde_json::json!({"ok": true, "reply": reply})
                    }
                    ka_agent_channels::commands::Outcome::Silent => {
                        serde_json::json!({"ok": true, "reply": ""})
                    }
                    ka_agent_channels::commands::Outcome::NotACommand => {
                        serde_json::json!({"error": "not a command"})
                    }
                }
            }
            (Some(req), Some("attach")) => {
                let since = req.get("since").and_then(|v| v.as_u64()).unwrap_or(0);
                return serve_attach(reader, w, ctx, since).await;
            }
            (Some(_), _) => serde_json::json!({"error": "unknown op"}),
            (None, _) => serde_json::json!({"error": "bad json"}),
        }
    };
    w.write_all(format!("{reply}\n").as_bytes()).await?;
    w.flush().await
}

/// `{"op":"attach","since":N}`: a live terminal on the host's session. The
/// server sends `hello`, then (when `since` names the last seq the client
/// saw) the replayed gap, then every live envelope — one JSON line each,
/// `seq` included. The client sends `say` lines that go through the shared
/// command router exactly like a channel message.
async fn serve_attach(
    mut reader: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    mut w: tokio::net::unix::OwnedWriteHalf,
    ctx: AdminCtx,
    since: u64,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::sync::broadcast::error::RecvError;
    // Subscribe (plus the replay snapshot) before the hello so nothing
    // between the two is lost; `since` 0 is live only.
    let (mut sub, replay) = if since > 0 {
        ctx.bus.subscribe_since(since)
    } else {
        (ctx.bus.subscribe(), Vec::new())
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
                break;
            }
        }
    });
    let send = |tx: &tokio::sync::mpsc::UnboundedSender<String>, ev: &Envelope| -> bool {
        match serde_json::to_string(ev) {
            Ok(json) => tx.send(format!("{json}\n")).is_ok(),
            Err(_) => true,
        }
    };
    match ctx.runtime.status_report() {
        Ok(report) => {
            send(
                &tx,
                &Envelope::new(
                    0,
                    AgentEvent::Hello {
                        report: Box::new(report),
                    },
                ),
            );
        }
        Err(e) => {
            send(
                &tx,
                &Envelope::new(
                    0,
                    AgentEvent::Error {
                        class: "internal".into(),
                        message: format!("status: {e}"),
                    },
                ),
            );
        }
    }
    // Events that raced the snapshot ride both the replay and the live
    // stream; remember the last replayed seq and skip it live.
    let mut last_sent = replay.last().map_or(0, |env| env.seq);
    for env in replay {
        if !send(&tx, &env) {
            break;
        }
    }
    let fwd_tx = tx.clone();
    let forwarder = tokio::spawn(async move {
        loop {
            let ev = match sub.recv().await {
                Ok(ev) if ev.seq > last_sent => {
                    last_sent = ev.seq;
                    ev
                }
                Ok(_) => continue,
                Err(RecvError::Lagged(n)) => std::sync::Arc::new(Envelope::new(
                    0,
                    AgentEvent::Notice {
                        text: format!("(missed {n} events)"),
                    },
                )),
                Err(RecvError::Closed) => break,
            };
            if !send(&fwd_tx, &ev) {
                break;
            }
        }
    });
    loop {
        let line = match read_bounded_line(&mut reader, MAX_ATTACH_LINE).await {
            Ok(Some(l)) => l,
            Ok(None) | Err(_) => break,
        };
        let Ok(req) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        // `status` answers this client with a fresh `hello`, ordered after
        // everything it said before (a piped chat waits for idle with it).
        if req.get("op").and_then(|o| o.as_str()) == Some("status") {
            if let Ok(report) = ctx.runtime.status_report() {
                send(
                    &tx,
                    &Envelope::new(
                        0,
                        AgentEvent::Hello {
                            report: Box::new(report),
                        },
                    ),
                );
            }
            continue;
        }
        let Some(text) = Some(req)
            .filter(|v| v.get("op").and_then(|o| o.as_str()) == Some("say"))
            .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
        else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match ka_agent_channels::commands::route(&ctx.runtime, "console", text).await {
            ka_agent_channels::commands::Outcome::Reply(r) => {
                send(&tx, &Envelope::new(0, AgentEvent::Reply { text: r }));
            }
            ka_agent_channels::commands::Outcome::Silent => {}
            ka_agent_channels::commands::Outcome::NotACommand => {
                let expanded =
                    ka_agent_channels::refs::expand(&ctx.store, &ctx.runtime.workspace(), text)
                        .await;
                if let Err(e) = ctx.runtime.handle_message(&expanded, "console").await {
                    send(
                        &tx,
                        &Envelope::new(
                            0,
                            AgentEvent::Error {
                                class: "internal".into(),
                                message: e.to_string(),
                            },
                        ),
                    );
                }
            }
        }
    }
    forwarder.abort();
    drop(tx);
    let _ = writer.await;
    Ok(())
}
