//! The worker side: runs inside the `ka-agent` (or test) binary, links the
//! pinned Ka crates, and hosts one engine per `open` over the NDJSON
//! protocol (`docs/contract/worker-protocol.md`).
//!
//! Trust invariant (SEED §3.1.5): only the allowlisted ka commands are
//! ever constructed here — `prompt`, `abort`, `answer`, `set_model`,
//! `set_mode`, `set_name`, `compact`. `shell`, `set_feature`,
//! `set_sandbox`, `save_api_key` and `save_settings` have no code path.
//!
//! Exit codes: 0 clean (`close` or stdin EOF), 3 engine closed without a
//! trailing `idle` (ka gap G13 — engine death is silent in ka, the worker
//! makes it loud), 1 anything reported through `Fatal`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ka_protocol::{AskId, Command, Event, ImagePart, Mode};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{Receiver, Sender};

use crate::proto::{StrandSel, WorkerCommand, WorkerEvent};
use crate::scenario::{Scenario, ScenarioSpeaker};

/// How long `close`/EOF drains the engine for a terminal `idle` before
/// exiting, so an aborted turn still persists to the strand (ka appends
/// per prompt cycle, gap G8).
const DRAIN_BUDGET: Duration = Duration::from_secs(2);

/// The scripted model used when a scenario drives the engine. The row is
/// overlaid onto the embedded catalog: `openai_chat` wire so the scripted
/// speaker owns it, unreachable base URL, priced at $1/Mtok input so the
/// engine's cost true-up yields `tokens / 1e6` dollars exactly.
const SCENARIO_CATALOG: &str = "[dialects.\"scen/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\npriced = true\n\n[dialects.\"scen/m\".price]\ninput_per_mtok = 1.0\noutput_per_mtok = 0.0\n";
const SCENARIO_MODEL: &str = "scen/m";

/// Worker entry configuration.
#[derive(Debug, Clone, Default)]
pub struct WorkerRun {
    /// Default scripted-speaker scenario (used when `open` carries none);
    /// the `--scenario` flag of the test worker feeds this.
    pub scenario: Option<PathBuf>,
}

/// Run the worker to completion on its own current-thread runtime (the
/// same shape as ka-harness's `main`). Returns `Err` for fatal worker
/// errors (the binary reports them and exits 1); the engine-closed
///-without-idle path exits the process with code 3 directly.
pub fn run_worker(run: WorkerRun) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to start runtime: {e}"))?;
    runtime.block_on(worker_main(run))
}

/// Parse a permission-mode word (`guarded|accept_edits|free|plan`).
pub fn parse_mode(s: &str) -> Option<Mode> {
    match s {
        "guarded" => Some(Mode::Guarded),
        "accept_edits" => Some(Mode::AcceptEdits),
        "free" => Some(Mode::Free),
        "plan" => Some(Mode::Plan),
        _ => None,
    }
}

/// The single line writer to the daemon: one NDJSON object per line,
/// written and flushed atomically so lines never interleave.
struct Out {
    stdout: Arc<tokio::sync::Mutex<tokio::io::Stdout>>,
}

impl Out {
    fn new() -> Self {
        Self {
            stdout: Arc::new(tokio::sync::Mutex::new(tokio::io::stdout())),
        }
    }

    async fn emit(&self, ev: WorkerEvent) {
        let Ok(line) = crate::proto::to_line(&ev) else {
            return;
        };
        let mut out = self.stdout.lock().await;
        let _ = out.write_all(line.as_bytes()).await;
        let _ = out.flush().await;
    }

    async fn emit_ka(&self, event: Event) {
        self.emit(WorkerEvent::Ka { event }).await;
    }
}

/// Session metadata kept beside the split channel ends (the command
/// sender and event receiver live as separate locals in `worker_main`,
/// so the select loop's event arm and the command handlers never fight
/// over one borrow): what the worker needs to resolve strand paths and
/// re-assert policy after `replay`.
struct EngineMeta {
    workspace: PathBuf,
    /// Model selector to re-assert after `replay` (validation F-1).
    assert_model: Option<String>,
    /// Mode to re-assert after `replay`.
    assert_mode: Option<Mode>,
}

async fn worker_main(run: WorkerRun) -> Result<(), String> {
    let out = Out::new();
    out.emit(WorkerEvent::Ready).await;

    // Cancel-safe line stream: `Lines` keeps partial reads inside the
    // stream (created once, here), so a select! branch cancelled
    // mid-line never drops command bytes — plain `read_line` buffers in
    // the future instead and loses them on cancellation.
    let mut stdin_lines = BufReader::new(tokio::io::stdin()).lines();
    let mut engine: Option<EngineMeta> = None;
    // Split channel ends for the select loop (see `EngineMeta` above):
    // the event arm owns `engine_rx`, the command handlers own
    // `engine_cmds`.
    let mut engine_cmds: Option<Sender<Command>> = None;
    let mut engine_rx: Option<Receiver<Event>> = None;
    let mut saw_session_info = false;
    let mut last_was_idle = false;
    let mut turn_live = false; // a prompt cycle is (potentially) running

    loop {
        tokio::select! {
            read = stdin_lines.next_line() => {
                let Some(line) = read.map_err(|e| format!("stdin: {e}"))? else {
                    // stdin EOF: the daemon is gone. Abort any live turn
                    // (draining so it persists), then exit cleanly.
                    if let Some(tx) = engine_cmds.as_ref() {
                        send_cmd(tx, Command::Abort).await;
                        drain_to_idle(&out, &mut engine_rx, turn_live).await;
                    }
                    return Ok(());
                };
                let cmd: WorkerCommand = match crate::proto::from_line(&line) {
                    Ok(cmd) => cmd,
                    Err(e) => {
                        eprintln!("worker: ignoring malformed command line: {e}");
                        continue;
                    }
                };
                match cmd {
                    WorkerCommand::Open { workspace, strand, model, mode, engine_layer_toml, scenario, auto_allow } => {
                        if engine.is_some() {
                            let msg = "open: engine already open".to_string();
                            out.emit(WorkerEvent::Fatal { message: msg.clone() }).await;
                            return Err(msg);
                        }
                        let overlay = OpenOverlay {
                            model,
                            mode,
                            engine_layer_toml,
                            scenario,
                            auto_allow: &auto_allow,
                        };
                        match open_engine(&workspace, &strand, overlay, &run).await {
                            Ok((meta, commands, events)) => {
                                engine = Some(meta);
                                engine_cmds = Some(commands);
                                engine_rx = Some(events);
                            }
                            Err(msg) => {
                                out.emit(WorkerEvent::Fatal { message: msg.clone() }).await;
                                return Err(msg);
                            }
                        }
                    }
                    WorkerCommand::Close => {
                        if let Some(tx) = engine_cmds.as_ref() {
                            send_cmd(tx, Command::Abort).await;
                            drain_to_idle(&out, &mut engine_rx, turn_live).await;
                        }
                        return Ok(());
                    }
                    other => {
                        let handled = forward(other, engine_cmds.as_ref()).await;
                        if matches!(handled, Handled::Prompt) {
                            turn_live = true;
                        }
                    }
                }
            }
            maybe_ev = async {
                match engine_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending::<Option<Event>>().await,
                }
            } => {
                let Some(ev) = maybe_ev else {
                    // The engine event channel closed. Without a trailing
                    // idle this is a crash (ka gap G13): say so loudly and
                    // exit 3 so the daemon can mark the cycle interrupted.
                    if !saw_session_info {
                        out.emit(WorkerEvent::Fatal {
                            message: "engine exited before bootstrap completed".to_string(),
                        })
                        .await;
                    } else if !last_was_idle {
                        out.emit(WorkerEvent::EngineClosed {
                            reason: Some("engine event channel closed without idle".to_string()),
                        })
                        .await;
                    }
                    std::process::exit(3);
                };
                last_was_idle = matches!(ev, Event::Idle);
                if last_was_idle {
                    turn_live = false;
                }
                match ev {
                    Event::SessionInfo { id } => {
                        saw_session_info = true;
                        out.emit_ka(Event::SessionInfo { id: id.clone() }).await;
                        let path = engine
                            .as_ref()
                            .map(|e| e.workspace.clone())
                            .and_then(|ws| strand_path(&ws, &id));
                        out.emit(WorkerEvent::Strand { id, path }).await;
                    }
                    other => {
                        let is_replay = matches!(other, Event::Replay { .. });
                        out.emit_ka(other).await;
                        if is_replay {
                            reassert_policy(&mut engine, engine_cmds.as_ref()).await;
                        }
                    }
                }
            }
        }
    }
}

/// Send one command, logging if the engine already went away.
async fn send_cmd(tx: &Sender<Command>, cmd: Command) {
    if tx.send(cmd).await.is_err() {
        eprintln!("worker: engine command channel closed");
    }
}

/// Resolve the strand file path for an id inside the worker's own
/// `KA_DATA_DIR` (the port pins it before spawn). `None` while the file
/// does not exist on disk yet — a fresh strand materializes only on its
/// first append.
fn strand_path(workspace: &Path, id: &str) -> Option<String> {
    match ka_strand::resolve_id(workspace, id) {
        Ok(ka_strand::IdMatch::Unique(summary)) => {
            Some(summary.path.to_string_lossy().into_owned())
        }
        _ => None,
    }
}

/// After `replay`, re-assert the requested model and mode. This both
/// pins policy for the session (a strand recorded in a looser mode would
/// otherwise restore loose) and persists fresh `Change` records, making
/// the next resume self-restoring (validation F-1 / probe V-10).
async fn reassert_policy(engine: &mut Option<EngineMeta>, commands: Option<&Sender<Command>>) {
    let (Some(eng), Some(tx)) = (engine.as_mut(), commands) else {
        return;
    };
    if let Some(selector) = eng.assert_model.take() {
        eprintln!(
            "worker: set_model {selector:?} re-asserted after replay (persists a Change record)"
        );
        send_cmd(tx, Command::SetModel { selector }).await;
    }
    if let Some(mode) = eng.assert_mode.take() {
        eprintln!(
            "worker: set_mode {} re-asserted after replay (persists a Change record)",
            mode_word(mode)
        );
        send_cmd(tx, Command::SetMode { mode }).await;
    }
    // the resulting model_changed / mode_changed / idle events flow back
    // through the event pump
}

fn mode_word(mode: Mode) -> &'static str {
    match mode {
        Mode::Guarded => "guarded",
        Mode::AcceptEdits => "accept_edits",
        Mode::Free => "free",
        Mode::Plan => "plan",
    }
}

/// The per-open overlay inputs (kept together: the engine config chain is
/// one concern).
struct OpenOverlay<'a> {
    model: Option<String>,
    mode: Option<String>,
    engine_layer_toml: Option<String>,
    scenario: Option<PathBuf>,
    auto_allow: &'a [String],
}

/// Compose the engine config and spawn the engine for one `open`.
async fn open_engine(
    workspace: &Path,
    strand: &StrandSel,
    overlay: OpenOverlay<'_>,
    run: &WorkerRun,
) -> Result<(EngineMeta, Sender<Command>, Receiver<Event>), String> {
    let OpenOverlay {
        model,
        mode,
        engine_layer_toml,
        scenario,
        auto_allow,
    } = overlay;
    // Config chain: ka defaults < engine layer < security overlay (always
    // last, always wins — SEED §3.4).
    let mut cfg = ka_engine::Config::default();
    if let Some(text) = &engine_layer_toml {
        let layer = ka_engine::Config::parse_layer(text, "ka-agent-engine-layer")
            .map_err(|e| format!("engine layer: {e}"))?;
        cfg.overlay(layer);
    }
    let requested_mode = match mode.as_deref() {
        None => None,
        Some(word) => Some(
            parse_mode(word)
                .ok_or_else(|| format!("unknown mode {word:?} (guarded|accept_edits|free|plan)"))?,
        ),
    };
    // Security overlay: explicit-or-guarded mode, bash auto-backgrounding
    // off (a detached job would escape cancellation, gap G15), and the
    // engine's `remember` hand off — memory proposals go through Ka
    // Agent (§4.4). The overlay always wins over the engine layer.
    let mode = requested_mode.unwrap_or(Mode::Guarded);
    cfg.mode = Some(mode);
    cfg.tools.bash.background_after_ms = Some(0);
    if !cfg.features.disable.iter().any(|s| s == "tool:remember") {
        cfg.features.disable.push("tool:remember".to_string());
    }
    cfg.cwd = Some(workspace.to_string_lossy().into_owned());

    // Approval profile: reads, proposals and local writes never ask. Exact
    // tool-name allow rules sit ahead of every other rule (first match
    // wins), so the operator's [[rules]] cannot re-open an ask for them.
    for name in auto_allow.iter().rev() {
        cfg.rules.insert(
            0,
            ka_engine::config::Rule {
                tool: name.clone(),
                pattern: None,
                verdict: ka_engine::config::Verdict::Allow,
            },
        );
    }
    // Self-protection: ka-agent's own files (config, store, secrets,
    // memory, other profiles) are off limits to the engine's file tools,
    // which pattern rules cannot express (`../config.toml` never matches a
    // raw-path pattern). The guard reads the protected lists from the
    // worker env; both lists ride the pre_tool_use hook contract.
    if std::env::var_os("KA_AGENT_GUARD_WRITE").is_some()
        && let Ok(exe) = std::env::current_exe()
    {
        for tool in ["write", "edit", "read", "grep"] {
            cfg.hooks.push(ka_engine::config::Hook {
                event: ka_engine::config::HookEvent::PreToolUse,
                tool: Some(tool.to_string()),
                command: format!("{:?} __guard", exe),
            });
        }
    }

    // The assistant's identity: the runtime compiles persona + house rules
    // into `<engine home>/.config/ka/IDENTITY.md` (HOME in this process);
    // the engine re-reads it per turn, so persona edits need no restart.
    if let Some(home) = std::env::var_os("HOME") {
        let identity = PathBuf::from(home).join(".config/ka/IDENTITY.md");
        if identity.exists() {
            cfg.prompt.identity_file = Some(identity.to_string_lossy().into_owned());
        }
    }

    // MCP bridge: the engine's kagent__* tools (memory_propose, notes_*).
    // Spawned as this same binary with `__mcp`; it inherits the worker's
    // sanitized env, including KA_AGENT_DATA_DIR for the store.
    //
    // The bridge only needs the data root: the tool calls themselves run
    // in the daemon, which reads secrets from its own store.
    if let Ok(exe) = std::env::current_exe() {
        let mut mcp_env = std::collections::HashMap::new();
        let data_dir = std::env::var_os("KA_AGENT_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(ka_agent_core::layout::default_root);
        mcp_env.insert(
            "KA_AGENT_DATA_DIR".to_string(),
            data_dir.to_string_lossy().into_owned(),
        );
        cfg.mcp.push(ka_engine::mcp::McpServerConfig {
            name: "kagent".to_string(),
            command: Some(exe.to_string_lossy().into_owned()),
            args: vec!["__mcp".to_string()],
            env: mcp_env,
            url: None,
            headers: Vec::new(),
        });
    }

    let strand_choice = match strand {
        StrandSel::New => ka_engine::StrandChoice::New,
        StrandSel::Path(p) => ka_engine::StrandChoice::Path(PathBuf::from(p)),
    };

    let scenario_path = scenario.or_else(|| run.scenario.clone());
    let (handle, assert_model) = if let Some(path) = scenario_path {
        // Scripted run: the scenario model must exist in the catalog for
        // the live (scripted) voice to be used at all — without a model
        // the engine falls back to the canned speaker and skips tools.
        let scenario = Scenario::load(&path)?;
        let mut catalog = ka_dialect::Catalog::embedded();
        let overlay = ka_dialect::Catalog::parse(SCENARIO_CATALOG)
            .map_err(|e| format!("internal: scenario catalog: {e}"))?;
        catalog.overlay(overlay);
        // only what `open` explicitly asked for is re-asserted after
        // replay; the derived scenario default is already in the config
        let effective = model.clone().unwrap_or_else(|| SCENARIO_MODEL.to_string());
        cfg.model = Some(effective);
        (
            ka_engine::spawn_with_speaker(
                cfg,
                catalog,
                strand_choice,
                ka_dialect::Wire::OpenaiChat,
                Arc::new(ScenarioSpeaker::new(scenario)),
            ),
            model,
        )
    } else {
        if let Some(model) = &model {
            cfg.model = Some(model.clone());
        }
        (
            // the pinned ka-dialect registers every provider ka-agent
            // knows (opencode-go included, with its native per-session
            // routing header) — the embedded catalog, parsed once per
            // process
            ka_engine::spawn_full(cfg, crate::catalog::catalog().clone(), strand_choice),
            model,
        )
    };

    Ok((
        EngineMeta {
            workspace: workspace.to_path_buf(),
            assert_model,
            assert_mode: requested_mode,
        },
        handle.commands,
        handle.events,
    ))
}

enum Handled {
    Prompt,
    Other,
}

/// Forward one allowlisted command to the engine. Everything outside the
/// allowlist is logged to stderr and dropped — the daemon has no way to
/// express the local-only commands (`shell`, `set_feature`,
/// `set_sandbox`, `save_*`) in the first place, and the worker refuses
/// to guess.
async fn forward(cmd: WorkerCommand, commands: Option<&Sender<Command>>) -> Handled {
    use WorkerCommand as W;

    let Some(tx) = commands else {
        eprintln!("worker: ignored {cmd:?} before open");
        return Handled::Other;
    };

    match cmd {
        W::Open { .. } | W::Close => unreachable!("handled by the caller"),
        W::Submit {
            text,
            images,
            skills,
        } => {
            send_cmd(
                tx,
                Command::Prompt {
                    text,
                    schema: None,
                    images: images
                        .into_iter()
                        .map(|i| ImagePart {
                            data: i.data_base64,
                            media_type: i.media_type,
                        })
                        .collect(),
                    allowed_tools: None,
                    model: None,
                    skills,
                },
            )
            .await;
            Handled::Prompt
        }
        W::Answer { ask_id, choice } => {
            send_cmd(
                tx,
                Command::Answer {
                    question: AskId(ask_id),
                    choice,
                },
            )
            .await;
            Handled::Other
        }
        W::Abort => {
            send_cmd(tx, Command::Abort).await;
            Handled::Other
        }
        W::SetModel { selector } => {
            send_cmd(tx, Command::SetModel { selector }).await;
            Handled::Other
        }
        W::SetMode { mode } => match parse_mode(&mode) {
            Some(mode) => {
                send_cmd(tx, Command::SetMode { mode }).await;
                Handled::Other
            }
            None => {
                eprintln!("worker: ignored set_mode with unknown mode {mode:?}");
                Handled::Other
            }
        },
        W::SetName { name } => {
            send_cmd(tx, Command::SetName { name }).await;
            Handled::Other
        }
        W::Compact => {
            send_cmd(tx, Command::Compact { focus: None }).await;
            Handled::Other
        }
    }
}

/// After `abort` (close or stdin EOF): keep pumping events until the
/// terminal `idle`, the channel closes, or the budget lapses — so the
/// daemon still sees `turn_finished{aborted}` and the strand is
/// persisted before the process goes away.
async fn drain_to_idle(out: &Out, engine_rx: &mut Option<Receiver<Event>>, turn_live: bool) {
    if !turn_live {
        return; // nothing in flight; the abort was a no-op
    }
    let Some(rx) = engine_rx.as_mut() else {
        return;
    };
    let deadline = tokio::time::Instant::now() + DRAIN_BUDGET;
    loop {
        let Ok(maybe) = tokio::time::timeout_at(deadline, rx.recv()).await else {
            return; // budget spent
        };
        let Some(ev) = maybe else {
            return; // engine gone
        };
        if matches!(ev, Event::Idle) {
            out.emit_ka(ev).await;
            return;
        }
        out.emit_ka(ev).await;
    }
}
