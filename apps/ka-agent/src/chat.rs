//! The terminal chat: `ka-agent chat` (and plain `ka-agent`). When the
//! background daemon runs, the chat attaches to it over the admin socket
//! and shares its session with every channel; otherwise it runs a local
//! runtime (holding the instance lock). Both modes render the same
//! [`SurfaceEvent`] stream and send through the same command router.

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ka_agent_runtime::{
    EventBus, InstanceLock, LockError, Runtime, Surface, SurfaceEvent, paths::Paths,
};
use ka_agent_store::Store;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;

use crate::admin;

/// How long a piped chat waits for its answers after stdin closes.
const EOF_WAIT: Duration = Duration::from_secs(120);

/// One event source + sink: a local runtime or the attached daemon.
enum Backend {
    Local {
        runtime: Runtime,
        store: Arc<Store>,
        rx: broadcast::Receiver<SurfaceEvent>,
        /// Router replies and status answers, delivered before bus events.
        pending: VecDeque<SurfaceEvent>,
        _lock: InstanceLock,
    },
    Attached {
        lines: admin::AttachLines,
        w: tokio::net::unix::OwnedWriteHalf,
    },
}

enum Incoming {
    Event(Box<SurfaceEvent>),
    Lagged(u64),
    Closed,
}

impl Backend {
    /// Build a local runtime over the data root (the daemon is not running).
    fn local(
        paths: &Paths,
        config: &Path,
        scenario: Option<PathBuf>,
    ) -> Result<Self, std::process::ExitCode> {
        // Strict, like the daemon: a console running on silently-defaulted
        // config would write to the real data root with a workspace the
        // operator never chose.
        let cfg = ka_agent_core::DaemonConfig::load(config).map_err(|e| {
            eprintln!(
                "ka-agent: config ({}) invalid: {e}\nrun `ka-agent setup` to create one",
                config.display()
            );
            std::process::ExitCode::FAILURE
        })?;
        let lock = match InstanceLock::acquire(paths) {
            Ok(l) => l,
            Err(LockError::Held(root)) => {
                crate::daemon::print_lock_held(&root);
                return Err(std::process::ExitCode::FAILURE);
            }
            Err(LockError::Io(e)) => {
                eprintln!(
                    "ka-agent: cannot lock {}: {e}",
                    paths.instance_lock().display()
                );
                return Err(std::process::ExitCode::FAILURE);
            }
        };
        let store = Store::open(&paths.state_db()).map(Arc::new).map_err(|e| {
            eprintln!("ka-agent: cannot open {}: {e}", paths.state_db().display());
            std::process::ExitCode::FAILURE
        })?;
        let bus = Arc::new(EventBus::new());
        // Subscribe before the runtime exists: its recovery notices count.
        let rx = bus.subscribe();
        let mut port = ka_agent_port::WorkerPort::with_current_exe();
        if let Some(sc) = scenario {
            port.scenario_override = Some(sc);
        }
        let runtime = Runtime::new(
            cfg,
            paths.clone(),
            Arc::clone(&store),
            port,
            bus as Arc<dyn Surface>,
        )
        .map_err(|e| {
            eprintln!("ka-agent: runtime init failed: {e}");
            std::process::ExitCode::FAILURE
        })?;
        runtime.set_config_path(config.to_path_buf());
        // No-agent cron handlers (M11): the calendar digest runs without the
        // engine. Registered right after the runtime exists, before any tick.
        {
            let st = Arc::clone(&store);
            let tz = runtime.config().time_zone();
            runtime.set_no_agent_handler(
                "calendar",
                Arc::new(move |src| ka_agent_channels::calendar::digest_in(src, &st, &tz)),
            );
        }
        let maintenance = runtime.clone();
        tokio::spawn(async move {
            maintenance.run_maintenance(Duration::from_secs(15)).await;
        });
        Ok(Self::Local {
            runtime,
            store,
            rx,
            pending: VecDeque::new(),
            _lock: lock,
        })
    }

    async fn attach(paths: &Paths) -> Result<Self, admin::AdminError> {
        let (lines, w) = admin::attach(paths).await?;
        Ok(Self::Attached { lines, w })
    }

    fn is_attached(&self) -> bool {
        matches!(self, Self::Attached { .. })
    }

    async fn next(&mut self) -> Incoming {
        match self {
            Self::Local { rx, pending, .. } => {
                // Real events first: a synthesized reply must never jump
                // ahead of bus events broadcast before it (an EOF-time
                // status answer, in particular).
                match rx.try_recv() {
                    Ok(ev) => return Incoming::Event(Box::new(ev)),
                    Err(broadcast::error::TryRecvError::Lagged(n)) => {
                        return Incoming::Lagged(n);
                    }
                    Err(broadcast::error::TryRecvError::Closed) => return Incoming::Closed,
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }
                if let Some(ev) = pending.pop_front() {
                    return Incoming::Event(Box::new(ev));
                }
                match rx.recv().await {
                    Ok(ev) => Incoming::Event(Box::new(ev)),
                    Err(broadcast::error::RecvError::Lagged(n)) => Incoming::Lagged(n),
                    Err(broadcast::error::RecvError::Closed) => Incoming::Closed,
                }
            }
            Self::Attached { lines, .. } => loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if let Ok(ev) = serde_json::from_str::<SurfaceEvent>(&line) {
                            return Incoming::Event(Box::new(ev));
                        }
                    }
                    Ok(None) | Err(_) => return Incoming::Closed,
                }
            },
        }
    }

    /// Send one operator line through the shared router.
    async fn say(&mut self, text: &str) -> Result<(), String> {
        match self {
            Self::Local {
                runtime,
                store,
                pending,
                ..
            } => {
                match ka_agent_channels::commands::route(runtime, "console", text).await {
                    ka_agent_channels::commands::Outcome::Reply(r) => {
                        pending.push_back(SurfaceEvent::Reply { text: r });
                    }
                    ka_agent_channels::commands::Outcome::Silent => {}
                    ka_agent_channels::commands::Outcome::NotACommand => {
                        let expanded =
                            ka_agent_channels::refs::expand(store, &runtime.workspace(), text)
                                .await;
                        if let Err(e) = runtime.handle_message(&expanded, "console").await {
                            pending.push_back(SurfaceEvent::Error {
                                class: "internal".into(),
                                message: e.to_string(),
                            });
                        }
                    }
                }
                Ok(())
            }
            Self::Attached { w, .. } => {
                let line = serde_json::json!({"op": "say", "text": text});
                write_line(w, &line).await
            }
        }
    }

    /// Ask for a fresh status; it arrives as a `Hello` event, ordered after
    /// everything said before it.
    async fn request_status(&mut self) -> Result<(), String> {
        match self {
            Self::Local {
                runtime, pending, ..
            } => {
                match runtime.status_report() {
                    Ok(report) => pending.push_back(SurfaceEvent::Hello { report }),
                    Err(e) => return Err(e.to_string()),
                }
                Ok(())
            }
            Self::Attached { w, .. } => write_line(w, &serde_json::json!({"op": "status"})).await,
        }
    }
}

async fn write_line(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    v: &serde_json::Value,
) -> Result<(), String> {
    w.write_all(format!("{v}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    w.flush().await.map_err(|e| e.to_string())
}

/// Terminal rendering state.
struct Term {
    /// Dim progress notes (stdout is a terminal and `NO_COLOR` is unset).
    color: bool,
}

impl Term {
    fn new() -> Self {
        Self {
            color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }
}

/// Print one event. The strings are the console's long-standing wording.
fn render(ev: &SurfaceEvent, out: &mut Term) {
    match ev {
        SurfaceEvent::Hello { .. } => {}
        SurfaceEvent::Reply { text } => println!("{text}"),
        SurfaceEvent::Inbound { source, text } => {
            if source != "console" {
                println!("\n» you ({source}): {text}");
            }
        }
        SurfaceEvent::TurnStarted => println!(),
        SurfaceEvent::Delta { text } => {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
        SurfaceEvent::Note { text } => {
            if out.color {
                println!("\n\x1b[2m· {text}\x1b[0m");
            } else {
                println!("\n· {text}");
            }
        }
        SurfaceEvent::Notice { text } => println!("\n· {text}"),
        SurfaceEvent::Approval {
            id,
            question,
            detail,
            expires_at_ms,
        } => {
            println!(
                "\n┌ approval {id} (expires in {})\n│ {question}",
                ka_agent_core::fmt_interval(expires_at_ms - ka_agent_core::now_ms())
            );
            if let Some(detail) = detail {
                for line in detail.lines().take(20) {
                    println!("│ {line}");
                }
            }
            println!("└ approve: /y   deny: /n");
        }
        SurfaceEvent::ApprovalSettled { id, status } => {
            println!("\n· approval {id} → {status}");
        }
        SurfaceEvent::TurnFinished {
            stop,
            cost_usd,
            tokens,
        } => println!("\n· turn {stop} · ${cost_usd:.4} · {tokens} tokens"),
        SurfaceEvent::Status { status } => println!("· session {status}"),
        SurfaceEvent::Error { class, message } => println!("\n! [{class}] {message}"),
    }
}

fn banner(model: Option<&str>, attached: bool, extra: &[String]) {
    let profile = ka_agent_core::layout::profile_name();
    let profile = if profile == "default" || std::env::var_os("KA_AGENT_DATA_DIR").is_some() {
        String::new()
    } else {
        format!(" [{profile}]")
    };
    println!(
        "Ka Agent{profile} · {} · {} · /help · Ctrl-C stops a reply, Ctrl-D quits",
        model.unwrap_or("no model"),
        if attached {
            "connected to the background service"
        } else {
            "local session"
        }
    );
    for line in extra {
        println!("{line}");
    }
}

/// Channels and web URL, for the attached banner (best effort).
fn attached_extras(paths: &Paths, config: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(cfg) = ka_agent_core::DaemonConfig::load(config) {
        let mut channels = Vec::new();
        if cfg.telegram.enabled {
            channels.push("Telegram");
        }
        if cfg.matrix.enabled {
            channels.push("Matrix");
        }
        if !channels.is_empty() {
            out.push(format!("also on: {}", channels.join(", ")));
        }
    }
    if paths.state_db().exists()
        && let Ok(store) = Store::open(&paths.state_db())
        && let Ok(Some(url)) = store.meta_get("web.url")
        && !url.is_empty()
    {
        out.push(format!("web: {url} (ka-agent dashboard)"));
    }
    out
}

/// Stdin on a plain thread (never blocks the async runtime or its
/// shutdown). Lines ending in `\` continue; `None` is end of input.
fn spawn_input(prompt: bool) -> tokio::sync::mpsc::UnboundedReceiver<Option<String>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut acc = String::new();
        loop {
            if prompt {
                print!("{}", if acc.is_empty() { "> " } else { "… " });
                let _ = std::io::stdout().flush();
            }
            let mut line = String::new();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    if !acc.is_empty() {
                        let _ = tx.send(Some(std::mem::take(&mut acc)));
                    }
                    let _ = tx.send(None);
                    return;
                }
                Ok(_) => {}
            }
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(head) = line.strip_suffix('\\') {
                acc.push_str(head);
                acc.push('\n');
                continue;
            }
            acc.push_str(line);
            if tx.send(Some(std::mem::take(&mut acc))).is_err() {
                return;
            }
        }
    });
    rx
}

pub async fn chat(config: PathBuf, scenario: Option<PathBuf>) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let mut backend = if admin::is_running(&paths) {
        match Backend::attach(&paths).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("ka-agent: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        match Backend::local(&paths, &config, scenario) {
            Ok(b) => {
                if let Backend::Local { runtime, .. } = &b {
                    banner(runtime.config().model.as_deref(), false, &[]);
                }
                b
            }
            Err(code) => return code,
        }
    };
    let attached = backend.is_attached();
    let mut term = Term::new();
    let mut input = spawn_input(std::io::stdin().is_terminal());
    let mut greeted = !attached;
    let mut busy = false;
    let mut eof_deadline: Option<tokio::time::Instant> = None;
    // The attach server greets with one `hello`; every later `hello` is a
    // reply to an explicit status request. Only those may end the EOF wait.
    let mut hello_seen = !attached;
    let mut awaiting_status = false;
    let mut last_ctrl_c: Option<Instant> = None;
    loop {
        let wait_until = eof_deadline
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(365 * 86_400));
        tokio::select! {
            incoming = backend.next() => match incoming {
                Incoming::Event(ev) => {
                    let ev = *ev;
                    match &ev {
                        SurfaceEvent::Hello { report } => {
                            if !greeted {
                                greeted = true;
                                banner(
                                    report.model.as_deref(),
                                    true,
                                    &attached_extras(&paths, &config),
                                );
                            } else if eof_deadline.is_some()
                                && awaiting_status
                                && report.status == "Idle"
                                && report.queue == 0
                            {
                                break;
                            }
                            hello_seen = true;
                            awaiting_status = false;
                        }
                        SurfaceEvent::TurnStarted => busy = true,
                        SurfaceEvent::Status { status } => {
                            if matches!(status.as_str(), "Idle" | "Paused" | "Degraded") {
                                busy = false;
                            }
                            awaiting_status = eof_deadline.is_some()
                                && backend.request_status().await.is_ok();
                            if !awaiting_status && eof_deadline.is_some() {
                                break;
                            }
                        }
                        SurfaceEvent::TurnFinished { .. } => {
                            awaiting_status = eof_deadline.is_some()
                                && backend.request_status().await.is_ok();
                            if !awaiting_status && eof_deadline.is_some() {
                                break;
                            }
                        }
                        _ => {}
                    }
                    render(&ev, &mut term);
                }
                Incoming::Lagged(n) => println!("· (missed {n} events)"),
                Incoming::Closed => {
                    if attached {
                        println!("\n· the background service closed the connection");
                    }
                    break;
                }
            },
            line = input.recv() => match line {
                Some(Some(text)) => {
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    if matches!(text.as_str(), "/quit" | "/q" | "/exit") {
                        break;
                    }
                    if !text.starts_with('/') {
                        busy = true;
                    }
                    if let Err(e) = backend.say(&text).await {
                        println!("! {e}");
                        break;
                    }
                }
                // End of input: let piped messages finish before leaving.
                Some(None) | None => {
                    if eof_deadline.is_none() {
                        eof_deadline = Some(tokio::time::Instant::now() + EOF_WAIT);
                        if backend.request_status().await.is_err() {
                            break;
                        }
                        awaiting_status = hello_seen;
                    }
                }
            },
            _ = tokio::signal::ctrl_c() => {
                let now = Instant::now();
                if last_ctrl_c.is_some_and(|t| now.duration_since(t) < Duration::from_secs(2)) {
                    break;
                }
                last_ctrl_c = Some(now);
                if busy {
                    let _ = backend.say("/stop").await;
                } else {
                    println!("\n(press Ctrl-C again to quit)");
                }
            }
            _ = tokio::time::sleep_until(wait_until), if eof_deadline.is_some() => break,
        }
    }
    std::process::ExitCode::SUCCESS
}

/// One message, one answer (setup's "send a test message"): attached when
/// the daemon runs, else a local runtime on the same code path as the chat.
pub async fn run_once(
    paths: &Paths,
    config: &Path,
    text: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut backend = if admin::is_running(paths) {
        Backend::attach(paths).await.map_err(|e| e.to_string())?
    } else {
        Backend::local(paths, config, None)
            .map_err(|_| "could not start a local session".to_string())?
    };
    let deadline = tokio::time::Instant::now() + timeout;
    backend.say(text).await?;
    // Ask for a status after the submit: an idle session with no turn means
    // the message never started one (a pre-flight refusal).
    backend.request_status().await?;
    let mut reply = String::new();
    let mut started = false;
    let mut last_problem: Option<String> = None;
    loop {
        let incoming = tokio::select! {
            i = backend.next() => i,
            _ = tokio::time::sleep_until(deadline) => return Err(format!("no reply after {}s", timeout.as_secs())),
        };
        match incoming {
            Incoming::Event(ev) => match *ev {
                SurfaceEvent::TurnStarted => started = true,
                SurfaceEvent::Delta { text } => {
                    started = true;
                    reply.push_str(&text);
                }
                SurfaceEvent::Error { message, .. } => last_problem = Some(message),
                SurfaceEvent::Notice { text } if !started => {
                    last_problem = Some(text);
                }
                SurfaceEvent::TurnFinished { stop, .. } => {
                    if reply.trim().is_empty() || stop == "error" {
                        return Err(
                            last_problem.unwrap_or_else(|| format!("the turn ended: {stop}"))
                        );
                    }
                    return Ok(reply);
                }
                SurfaceEvent::Hello { report }
                    if !started && report.status == "Idle" && report.queue == 0 =>
                {
                    return Err(last_problem.unwrap_or_else(|| "the message was not sent".into()));
                }
                _ => {}
            },
            Incoming::Lagged(_) => {}
            Incoming::Closed => return Err("the connection closed".into()),
        }
    }
}
