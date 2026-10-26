//! The terminal chat: `ka-agent chat` (and plain `ka-agent`). When the
//! background daemon runs, the chat attaches to it over the admin socket
//! and shares its session with every channel; otherwise it runs a local
//! runtime (holding the instance lock). Both modes render the same
//! [`Envelope`] stream and send through the same command router.

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::{Envelope, paths::Paths};
use ka_agent_store::Store;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;

use crate::admin;

/// How long a piped chat waits for its answers after stdin closes.
const EOF_WAIT: Duration = Duration::from_secs(120);

/// One event source + sink: a local runtime or the attached daemon.
enum Backend {
    Local {
        host: crate::host::Host,
        rx: broadcast::Receiver<std::sync::Arc<Envelope>>,
        /// Router replies and status answers, delivered before bus events.
        pending: VecDeque<Envelope>,
    },
    Attached {
        lines: admin::AttachLines,
        w: crate::ipc::IpcWrite,
    },
}

impl Backend {
    /// Stop a local host cleanly (tasks down, admin socket removed).
    fn close(self) {
        match self {
            Self::Local { host, .. } => host.shutdown(),
            Self::Attached { .. } => {}
        }
    }
}

enum Incoming {
    Event(std::sync::Arc<Envelope>),
    Lagged(u64),
    Closed,
}

impl Backend {
    /// Build a local runtime over the data root (the daemon is not running):
    /// a host without phone channels or web, serving the admin socket so a
    /// second terminal's `ka-agent status` answers while the chat runs.
    fn local(
        _paths: &Paths,
        config: &Path,
        scenario: Option<PathBuf>,
    ) -> Result<Self, std::process::ExitCode> {
        let mut host = crate::host::Host::start(
            config,
            crate::host::HostOptions {
                channels: false,
                web_port: None,
                scenario,
            },
        )
        .map_err(|e| {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        })?;
        let rx = host.take_early_rx().unwrap_or_else(|| host.bus.subscribe());
        Ok(Self::Local {
            host,
            rx,
            pending: VecDeque::new(),
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
                    Ok(ev) => return Incoming::Event(ev),
                    Err(broadcast::error::TryRecvError::Lagged(n)) => {
                        return Incoming::Lagged(n);
                    }
                    Err(broadcast::error::TryRecvError::Closed) => return Incoming::Closed,
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }
                if let Some(env) = pending.pop_front() {
                    return Incoming::Event(std::sync::Arc::new(env));
                }
                match rx.recv().await {
                    Ok(ev) => Incoming::Event(ev),
                    Err(broadcast::error::RecvError::Lagged(n)) => Incoming::Lagged(n),
                    Err(broadcast::error::RecvError::Closed) => Incoming::Closed,
                }
            }
            Self::Attached { lines, .. } => loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if let Ok(env) = serde_json::from_str::<Envelope>(&line) {
                            return Incoming::Event(std::sync::Arc::new(env));
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
            Self::Local { host, pending, .. } => {
                let (runtime, store) = (&host.runtime, &host.store);
                match ka_agent_channels::commands::route(runtime, "console", text).await {
                    ka_agent_channels::commands::Outcome::Reply(r) => {
                        pending.push_back(Envelope::new(0, AgentEvent::Reply { text: r }));
                    }
                    ka_agent_channels::commands::Outcome::Silent => {}
                    ka_agent_channels::commands::Outcome::NotACommand => {
                        let expanded =
                            ka_agent_channels::refs::expand(store, &runtime.workspace(), text)
                                .await;
                        if let Err(e) = runtime.handle_message(&expanded, "console").await {
                            pending.push_back(Envelope::new(
                                0,
                                AgentEvent::Error {
                                    class: "internal".into(),
                                    message: e.to_string(),
                                },
                            ));
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
            Self::Local { host, pending, .. } => {
                match host.runtime.status_report() {
                    Ok(report) => pending.push_back(Envelope::new(
                        0,
                        AgentEvent::Hello {
                            report: Box::new(report),
                        },
                    )),
                    Err(e) => return Err(e.to_string()),
                }
                Ok(())
            }
            Self::Attached { w, .. } => write_line(w, &serde_json::json!({"op": "status"})).await,
        }
    }
}

async fn write_line(w: &mut crate::ipc::IpcWrite, v: &serde_json::Value) -> Result<(), String> {
    w.write_all(format!("{v}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    w.flush().await.map_err(|e| e.to_string())
}

/// Terminal rendering state.
struct Term {
    /// Dim progress notes and rendered Markdown (stdout is a terminal and
    /// `NO_COLOR` is unset). Off, replies print verbatim — pipes and tests
    /// see exactly what the model wrote (minus control characters).
    color: bool,
    /// The reply line being streamed (printed raw as it arrives, rewritten
    /// rendered once its newline lands).
    line: String,
    /// Inside a fenced code block.
    in_fence: bool,
    /// When the current turn started (for the footer's duration).
    turn_started: Option<std::time::Instant>,
}

impl Term {
    fn new() -> Self {
        Self {
            color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
            line: String::new(),
            in_fence: false,
            turn_started: None,
        }
    }

    /// Stream a reply delta: text shows immediately; each completed line is
    /// redrawn with its Markdown rendered.
    fn delta(&mut self, text: &str) {
        use ka_agent_core::markdown::{Target, escape};
        if !self.color {
            print!("{}", escape(text, Target::Ansi));
            let _ = std::io::stdout().flush();
            return;
        }
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            print!("{}", escape(part, Target::Ansi));
            self.line.push_str(part);
            if parts.peek().is_some() {
                self.complete_line();
                println!();
            }
        }
        let _ = std::io::stdout().flush();
    }

    /// Redraw the pending line rendered (before other output or at the end
    /// of a reply); a no-op when nothing is pending.
    fn settle(&mut self) {
        if self.color && !self.line.is_empty() {
            self.complete_line();
            let _ = std::io::stdout().flush();
        }
    }

    /// Replace the raw echo of `self.line` (possibly soft-wrapped over
    /// several rows) with its rendered form; leaves the cursor at its end.
    fn complete_line(&mut self) {
        use ka_agent_core::markdown::{
            Target, fence_lang, render_code_line, render_fence_open, render_line,
        };
        let line = std::mem::take(&mut self.line);
        let rendered = if self.in_fence {
            if line.trim() == "```" {
                self.in_fence = false;
                "\x1b[2m──\x1b[22m".to_string()
            } else {
                render_code_line(&line, Target::Ansi)
            }
        } else if let Some(rest) = line.trim_start().strip_prefix("```") {
            self.in_fence = true;
            render_fence_open(fence_lang(rest))
        } else {
            render_line(&line, Target::Ansi)
        };
        let cols = terminal_size::terminal_size()
            .map(|(w, _)| usize::from(w.0).max(1))
            .unwrap_or(80);
        let width = line.chars().count();
        let rows = width.div_ceil(cols).max(1);
        if rows > 1 {
            print!("\x1b[{}A", rows - 1);
        }
        print!("\r\x1b[J{rendered}");
    }

    /// A reply ended: render what is left and forget fence state.
    fn end_reply(&mut self) {
        self.settle();
        self.in_fence = false;
    }
}

/// Print one envelope. The strings are the console's long-standing wording.
fn render(env: &Envelope, out: &mut Term) {
    if !matches!(env.ev, AgentEvent::Delta { .. }) {
        out.settle();
    }
    match &env.ev {
        AgentEvent::Hello { .. } => {}
        AgentEvent::Reply { text } => println!("{text}"),
        AgentEvent::Inbound { source, text } => {
            if source != "console" {
                println!("\n» you ({source}): {text}");
            }
        }
        AgentEvent::TurnStarted { .. } => {
            out.turn_started = Some(std::time::Instant::now());
            println!();
        }
        AgentEvent::Delta { text } => out.delta(text),
        AgentEvent::Note { text } => {
            if out.color {
                println!("\n\x1b[2m· {text}\x1b[0m");
            } else {
                println!("\n· {text}");
            }
        }
        AgentEvent::Notice { text } => println!("\n· {text}"),
        AgentEvent::Reminder { text, .. } => println!("\n⏰ {text}"),
        AgentEvent::File {
            path,
            name,
            caption,
        } => {
            let cap = if caption.is_empty() {
                String::new()
            } else {
                format!(" — {caption}")
            };
            let _ = name;
            println!("\n📎 {path}{cap}");
        }
        AgentEvent::SkillProposal {
            id,
            name,
            description,
        } => {
            println!(
                "\n💡 skill proposal {name}: {description}\n   /skills accept {id} · /skills reject {id}"
            );
        }
        AgentEvent::Approval {
            id,
            question,
            detail,
            expires_at_ms,
            ..
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
        AgentEvent::ApprovalSettled { id, status } => {
            println!("\n· approval {id} → {status}");
        }
        // Today's wording: the old Surface default turned a proposal into
        // a notice naming the two commands.
        AgentEvent::MemoryProposal { id, text } => {
            let short = ka_agent_core::ids::short_id(id);
            println!(
                "\n· 💡 remember this? {text}\n   /memory accept {short} · /memory reject {short}"
            );
        }
        AgentEvent::TurnFinished {
            stop,
            cost_usd,
            tokens,
            ..
        } => {
            out.end_reply();
            let took = out
                .turn_started
                .take()
                .map(|t| format!(" · {:.1}s", t.elapsed().as_secs_f64()))
                .unwrap_or_default();
            println!("\n· turn {stop}{took} · ${cost_usd:.4} · {tokens} tokens");
        }
        // Only states that need the operator: a settled turn is already
        // announced by its footer.
        AgentEvent::Session { status } => match status.as_str() {
            "Paused" => println!("· ⏸ paused — /resume to continue (budget or provider key)"),
            "Degraded" => println!("· ⚠ degraded — the worker keeps crashing; /new starts fresh"),
            _ => {}
        },
        AgentEvent::Error { class, message } => println!("\n! [{class}] {message}"),
        // Thinking, tool structure, plans and context meters are not drawn
        // here yet (today's console fidelity).
        AgentEvent::Thought { .. }
        | AgentEvent::ToolStarted { .. }
        | AgentEvent::ToolFinished { .. }
        | AgentEvent::Plan { .. }
        | AgentEvent::Context { .. }
        | AgentEvent::Compacting { .. }
        | AgentEvent::FinalChunk { .. }
        | AgentEvent::CronFinished { .. }
        | AgentEvent::Queue { .. }
        | AgentEvent::Settings { .. } => {}
    }
}

fn banner(model: Option<&str>, attached: bool, extra: &[String]) {
    let profile = ka_agent_core::layout::profile_name();
    let profile = if profile == "default" || std::env::var_os("KA_AGENT_DATA_DIR").is_some() {
        String::new()
    } else {
        format!(" [{profile}]")
    };
    let name = persona_name();
    let mode = mode_badge();
    println!(
        "☥ {name}{profile} · {}{mode} · {} · /help · Ctrl-C stops a reply, Ctrl-D quits",
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
    // `work> ` under a non-default profile: never type into the wrong one.
    let profile = ka_agent_core::layout::profile_name();
    let head = if profile == "default" || std::env::var_os("KA_AGENT_DATA_DIR").is_some() {
        "> ".to_string()
    } else {
        format!("{profile}> ")
    };
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut acc = String::new();
        loop {
            if prompt {
                print!("{}", if acc.is_empty() { &head } else { "… " });
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

/// A card the draft's bare `y`/`n`/`s` may answer: `(kind, id)`, kind one of
/// `approval`, `memory`, `skill`.
type PendingCard = (String, String);

/// Translate one submitted line against the armed card. Returns the line to
/// send and whether the submission answered the card. An answered card is
/// discharged: a later bare `y` is a message to the model, not a stale
/// command against a card that is already settled.
fn resolve_submission(text: &str, pending: Option<&PendingCard>) -> (String, bool) {
    let bare = text.trim();
    let Some((kind, id)) = pending else {
        return (text.to_string(), false);
    };
    match bare {
        "y" | "n" | "s" => {
            let line = match (bare, kind.as_str()) {
                ("y", "memory") => format!("/memory accept {id}"),
                ("n", "memory") => format!("/memory reject {id}"),
                ("y", "skill") => format!("/skills accept {id}"),
                ("n", "skill") => format!("/skills reject {id}"),
                ("s", _) => format!("/y {id} session"),
                (verb, _) => format!("/{verb} {id}"),
            };
            (line, true)
        }
        _ if answers_card(bare, kind, id) => (text.to_string(), true),
        _ => (text.to_string(), false),
    }
}

/// True when a full-form command addresses the armed card: `/y <id>` /
/// `/n <id>` (an approval), `/memory accept|reject <id>` (a memory card),
/// `/skills accept|reject <id>` (a skill card).
fn answers_card(text: &str, kind: &str, id: &str) -> bool {
    use ka_agent_core::ids::id_matches;
    let mut parts = text.split_whitespace();
    let Some(command) = parts.next() else {
        return false;
    };
    match command {
        "/y" | "/n" if kind == "approval" => {
            parts.next().is_some_and(|target| id_matches(id, target))
        }
        "/memory" if kind == "memory" => {
            matches!(parts.next(), Some("accept" | "reject"))
                && parts.next().is_some_and(|target| id_matches(id, target))
        }
        "/skills" if kind == "skill" => {
            matches!(parts.next(), Some("accept" | "reject"))
                && parts.next().is_some_and(|target| id_matches(id, target))
        }
        _ => false,
    }
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
                if let Backend::Local { host, .. } = &b {
                    banner(host.runtime.config().model.as_deref(), false, &[]);
                }
                b
            }
            Err(code) => return code,
        }
    };
    let attached = backend.is_attached();
    let mut term = Term::new();
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    // The inline client (plan 3.1) takes over the same loop when both ends
    // are TTYs; the piped path keeps the line reader untouched.
    let mut terminal = if interactive {
        let ws = match &backend {
            Backend::Local { host, .. } => host.runtime.workspace(),
            Backend::Attached { .. } => ka_agent_core::DaemonConfig::load(&config)
                .map(|c| c.workspace_path())
                .unwrap_or_else(|_| std::path::PathBuf::from(".")),
        };
        crate::term::Terminal::start(ws, persona_name(), String::new())
    } else {
        None
    };
    // The newest card the draft's `y`/`n`/`s` may answer; discharged once a
    // submission addresses it (or its approval settles).
    let mut pending_card: Option<PendingCard> = None;
    // One reader only: the terminal owns stdin when it is active.
    let mut input = if interactive {
        None
    } else {
        Some(spawn_input(std::io::stdin().is_terminal()))
    };
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
                Incoming::Event(env) => {
                    let ev = &env.ev;
                    match ev {
                        AgentEvent::Hello { report } => {
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
                        AgentEvent::TurnStarted { .. } => busy = true,
                        AgentEvent::Session { status } => {
                            if matches!(status.as_str(), "Idle" | "Paused" | "Degraded") {
                                busy = false;
                            }
                            awaiting_status = eof_deadline.is_some()
                                && backend.request_status().await.is_ok();
                            if !awaiting_status && eof_deadline.is_some() {
                                break;
                            }
                        }
                        AgentEvent::TurnFinished { .. } => {
                            awaiting_status = eof_deadline.is_some()
                                && backend.request_status().await.is_ok();
                            if !awaiting_status && eof_deadline.is_some() {
                                break;
                            }
                        }
                        _ => {}
                    }
                    match &env.ev {
                        AgentEvent::Approval { id, .. } => {
                            pending_card = Some(("approval".into(), id.clone()))
                        }
                        AgentEvent::ApprovalSettled { id, .. }
                            if pending_card.as_ref().is_some_and(|(_, c)| c == id) =>
                        {
                            pending_card = None;
                        }
                        AgentEvent::MemoryProposal { id, .. } => {
                            pending_card = Some(("memory".into(), id.clone()))
                        }
                        AgentEvent::SkillProposal { id, .. } => {
                            pending_card = Some(("skill".into(), id.clone()))
                        }
                        _ => {}
                    }
                    if let Some(t) = terminal.as_mut() {
                        crate::term::show_event(t, &env);
                        crate::term::cards::print_card(&env);
                    } else {
                        render(&env, &mut term);
                    }
                }
                Incoming::Lagged(n) => println!("· (missed {n} events)"),
                Incoming::Closed => {
                    if attached {
                        println!("\n· the background service closed the connection");
                    }
                    break;
                }
            },
            action = async {
                match terminal.as_mut() {
                    Some(t) => t.next_action(busy).await,
                    None => std::future::pending().await,
                }
            } => match action {
                Some(crate::term::Action::Submit(text)) => {
                    let (text, answered) = resolve_submission(&text, pending_card.as_ref());
                    if answered {
                        pending_card = None;
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
                Some(crate::term::Action::Interrupt) => {
                    let _ = backend.say("/stop").await;
                    busy = false;
                }
                Some(crate::term::Action::Eof) | None => break,
            },
            line = async {
                match input.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match line {
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
            _ = tokio::signal::ctrl_c(), if terminal.is_none() => {
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
    let attached = backend.is_attached();
    backend.close();
    let name = persona_name();
    if attached {
        println!("{name} keeps watch in the background.");
    } else {
        println!("{name} rests.");
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
    let outcome = run_once_turn(&mut backend, text, deadline, timeout).await;
    backend.close();
    outcome
}

/// The turn itself, on a backend the caller closes afterwards.
async fn run_once_turn(
    backend: &mut Backend,
    text: &str,
    deadline: tokio::time::Instant,
    timeout: Duration,
) -> Result<String, String> {
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
            Incoming::Event(env) => match &env.ev {
                AgentEvent::TurnStarted { .. } => started = true,
                AgentEvent::Delta { text } => {
                    started = true;
                    reply.push_str(text);
                }
                AgentEvent::Error { message, .. } => last_problem = Some(message.clone()),
                AgentEvent::Notice { text } if !started => {
                    last_problem = Some(text.clone());
                }
                AgentEvent::TurnFinished { stop, .. } => {
                    if reply.trim().is_empty() || stop == "error" {
                        return Err(
                            last_problem.unwrap_or_else(|| format!("the turn ended: {stop}"))
                        );
                    }
                    return Ok(reply);
                }
                AgentEvent::Hello { report }
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

/// The assistant's display name (`Ka` by default), read from the store.
fn persona_name() -> String {
    let path = ka_agent_core::layout::base_dir()
        .join("memory")
        .join("persona.md");
    std::fs::read_to_string(path)
        .ok()
        .map(|t| ka_agent_memory::persona::persona_name(&t))
        .unwrap_or_else(|| "Ka".to_string())
}

/// ` · mode <x>` when the mode is not the guarded default.
fn mode_badge() -> String {
    let mode = ka_agent_core::DaemonConfig::load(&crate::config_path(&None))
        .map(|c| c.mode)
        .unwrap_or_else(|_| "guarded".to_string());
    if mode == "guarded" {
        String::new()
    } else {
        format!(" · mode {mode}")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn card(kind: &str, id: &str) -> PendingCard {
        (kind.to_string(), id.to_string())
    }

    #[test]
    fn a_bare_answer_is_only_rewritten_while_a_card_is_armed() {
        let memory = card("memory", "deadbeefcafe");
        assert_eq!(
            resolve_submission("y", Some(&memory)),
            ("/memory accept deadbeefcafe".to_string(), true)
        );
        assert_eq!(
            resolve_submission("n", Some(&memory)),
            ("/memory reject deadbeefcafe".to_string(), true)
        );
        let skill = card("skill", "s1");
        assert_eq!(
            resolve_submission("y", Some(&skill)),
            ("/skills accept s1".to_string(), true)
        );
        let approval = card("approval", "a1");
        assert_eq!(
            resolve_submission("s", Some(&approval)),
            ("/y a1 session".to_string(), true)
        );
        // Once answered the card is discharged: the same bare `y` reaches the
        // model unchanged instead of becoming a command against a stale id.
        assert_eq!(resolve_submission("y", None), ("y".to_string(), false));
        // Surrounding whitespace still counts as the shortcut.
        assert!(resolve_submission("  y \n", Some(&memory)).1);
    }

    #[test]
    fn a_full_command_that_answers_the_card_discharges_it() {
        let memory = card("memory", "abcdef123456");
        assert_eq!(
            resolve_submission("/memory accept abcdef123456", Some(&memory)),
            ("/memory accept abcdef123456".to_string(), true)
        );
        // Short ids resolve, as the card prints them.
        assert!(resolve_submission("/memory reject abcdef", Some(&memory)).1);
        // A message that does not address the card leaves it armed.
        assert_eq!(
            resolve_submission("what is on my calendar?", Some(&memory)),
            ("what is on my calendar?".to_string(), false)
        );
        // The wrong command family or another id does not answer it.
        assert!(!resolve_submission("/skills reject abcdef", Some(&memory)).1);
        assert!(!resolve_submission("/memory accept 999999", Some(&memory)).1);
        // `/y` answers approvals, not proposals: the card stays armed.
        assert!(!resolve_submission("/y abcdef", Some(&memory)).1);
        let approval = card("approval", "aa11bb22");
        assert!(resolve_submission("/y aa11bb22", Some(&approval)).1);
        assert!(resolve_submission("/y aa11bb22 session", Some(&approval)).1);
        assert!(!resolve_submission("/memory accept aa11bb22", Some(&approval)).1);
    }
}
