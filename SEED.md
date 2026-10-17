# Ka Agent — Seed Document

Status: seed, 2026-10-15. Audience: whoever builds the first Ka Agent commits.

Conventions used throughout:

- **[Requirement]**: must hold for the MVP and every later phase. Changing one needs a written decision record (`docs/adr/`).
- **[Provisional]**: the current best decision. It may change after the validation named next to it (usually the Phase 0 spike).
- Evidence: paths such as `crates/ka-harness/src/serve.rs:241-277` point into the Ka repository (`github.com/jaltez/ka-harness`, renamed from `ka`) at commit `a56aae2`. That is workspace version 0.3.0, which is not yet released; the `ka` binary installed on the analysis machine was 0.2.8. Everything was established by reading source; nothing in Ka was built or run beyond `--version`. Claims marked *(code-read inference)* follow from the code but were not exercised.
- Ka's own vocabulary is kept where it exists: strand, record, digest, Hands, clearance, hardstops.

## 1. Product thesis

### 1.1 What Ka Agent is and the problem it solves

Ka is a coding agent that lives for one terminal session. When the session ends, nothing keeps working on your behalf: nothing remembers you across projects, answers from your phone, runs at 07:00, or waits for your approval while you are away.

Ka Agent is a separate, optional, resident assistant. It is a long-running process owned by one person. It provides:

- channels, starting with Telegram;
- durable identity, memory and history;
- tasks and approvals;
- an extension model (skills, plugins, connectors).

It delegates all model reasoning and tool execution to Ka engines. The problem it solves is **continuity with control**: an assistant that persists and acts across time and channels, whose memory, permissions and external actions the user can inspect, edit and veto.

### 1.2 Ka vs Ka Agent, and the Ka / Ba / Akh naming

| | Ka (engine, instantiated) | Ka Agent (resident assistant) |
|---|---|---|
| What | Turn engine, tools, permissions, providers, strands; CLI/TUI; ≤10 MB binary, no daemons (`AGENTS.md:3`) | Daemon, channels, memory, scheduler, approvals, plugin host |
| Lifetime | One engine per session, while a surface holds it | Always on; engines are started and stopped by Ka Agent |
| State | Strand files (append-only JSONL) | Ka Agent state DB, memory files, channel bindings |
| Ships as | `ka` binary + crates | Separate repository and binary (`ka-agent`) |

The naming follows Ka's ratified decision record (`research/ka/agent-boundary.md:15-19`):

- **Ka** is the vital essence: the engine every surface shares. In Ka Agent, one Ka is one engine instance bound to one strand.
- **Ba** is the mobile personality: identity, persona, memory, channels and presence. It moves between channels and outlives any single engine.
- **Akh** is the effective spirit born of their union: a running Agent instance in which Ba is bound to live Ka engines and acts in the world.
- **[Requirement]** Ba and Akh are layer names in docs, logs and UI copy, not crate names, as Ka's record already states. The product name is "Ka Agent" (two words).

### 1.3 Principles

- **Ka stays lightweight.** Ka Agent never adds code, crates, features or dependencies to the Ka repository or the `ka` binary. Ka enforces this on its side with a dependency allowlist gate (`crates/xtask/deps-allowlist.txt:1-5`, `research/ka/agent-boundary.md:21-40`).
- **Ka Agent is optional.** Ka must keep working exactly as it does without Ka Agent installed. Ka Agent works against unmodified, released Ka versions.
- **The user controls memory, permissions and actions.**
  - Stable memory is plain Markdown the user can read and edit.
  - Every grant is explicit and revocable.
  - Every action with external effects is approved, logged and auditable.
  - Nothing leaves the machine except model API calls and the configured channels.
- **Fail closed and boring.** Unanswered approvals deny. Unknown state is reported, never guessed. Interrupted work is never auto-replayed. This mirrors Ka's own "fail toward boring" rule (`AGENTS.md:40`).
- **Small and verifiable first.** One user, one channel, one workspace, then expand only behind exit criteria (§9, §10).

### 1.4 Positioning vs Hermes Agent and OpenClaw

Both products show that a resident, channel-facing agent is a real product category. Ka Agent takes conceptual inspiration from their publicly documented capabilities. It copies no implementation, prompts or internal structure; no source code of either was read for this document.

- **Hermes Agent** (Nous Research, MIT) is described publicly as a self-hosted agent with persistent memory, self-created skills with an optional review gate, a messaging gateway (Telegram, Discord, Slack, WhatsApp, Signal, email, CLI), natural-language scheduling and a layered security model. That model includes allowlists, approval of dangerous commands with deny-on-timeout, and deny-by-default for cron and webhook runs ([site](https://hermes-agent.nousresearch.com/), [memory](https://hermes-agent.nousresearch.com/docs/user-guide/features/memory), [security](https://hermes-agent.nousresearch.com/docs/user-guide/security)).
- **OpenClaw** (MIT, OpenClaw Foundation) is described publicly as a self-hosted gateway that connects many chat apps to an always-on agent. It has skills, a plugin/skill marketplace, cron and webhooks, loopback binding and DM pairing by default, and a stated "one gateway = one trust boundary" model ([docs](https://docs.openclaw.ai/), [security](https://docs.openclaw.ai/gateway/security)).

Ka Agent differs deliberately:

1. **Engine separation.** Reasoning, tools and permissions come from Ka, which stays a ≤10 MB coding engine with its own release cycle. Ka Agent is the resident layer around it.
2. **Narrow by design.** One operator per instance is the stated trust boundary, there is no marketplace in early phases, and Telegram is the only channel at first.
3. **Approval-first.** Effects outside the workspace require approval; Ka's own permission asks surface as chat approvals.
4. **Local-first and inspectable.** Memory is Markdown, operational state is SQLite, and there is no hosted component.
5. **Model-agnostic through Ka.** Anthropic Messages, OpenAI Chat and OpenAI Responses wires, plus local discovery (`crates/ka-dialect/src/lib.rs:26-35`, `crates/ka-dialect/src/discovery.rs:260-280`).

## 2. Core concepts

### 2.1 Agent instance

One running Ka Agent daemon (an Akh) plus its home directory: configuration, state database, memory files, attachments, and the isolated Ka data directory its engines use. One instance serves exactly one human operator; that is the trust boundary. Two operators need two instances.

### 2.2 Session / strand

- A **strand** is Ka's session: one append-only JSONL file whose replay reproduces engine state (`crates/ka-strand/src/lib.rs:61-64`).
- A **Ka Agent session** is Ka Agent's durable handle for a conversation thread. It has a Ka Agent id and is bound to exactly one strand at a time (by strand id and path), plus a workspace and status.
- The lineage moves to a new strand only on `/new`, fork or transfer.
- Ka Agent never writes strand files; only the Ka engine does.

### 2.3 Persistent memory

Durable knowledge about the operator and their world: preferences, projects, environment, conventions and facts. It is stored as Markdown files owned by Ka Agent, written only through an explicit accept step, and injected into engines through Ka's existing `MEMORY.md` convention (§4.4).

### 2.4 Episodic history

What happened, and when: sessions, prompt cycles, final answers, outcomes, costs and errors. It is indexed by Ka Agent for search and links back to the authoritative strand for full detail. It is history, not instruction: episodic content never becomes stable memory without acceptance.

### 2.5 Working state

The transient state of an active session:

- the in-flight prompt cycle;
- a pending Ka ask;
- queued inbound messages;
- streaming message handles;
- the worker process id.

The parts needed for recovery (queue, turn status, pending approvals) are persisted. The rest is rebuilt or declared lost on restart.

### 2.6 Skills

Reusable instructions and procedures in the ecosystem-standard `SKILL.md` format that Ka already discovers and lists (`research/ka/architecture.md:191-192`, `crates/ka-engine/src/conventions.rs:228-252`). A skill is text, not code. It changes what the model knows how to do, never what it is allowed to do.

### 2.7 Plugins

Installable units of capability. A plugin ships a manifest that declares its tools, effects, scopes, required secrets and network destinations. Installing a plugin never grants anything by itself; scopes are granted separately by the operator.

### 2.8 Connectors

Plugins that integrate an external service: GitHub, a calendar, an RSS feed, an HTTP API. A connector owns credentials for that service and exposes tools whose effects are classified (read, local write, external write, exec, irreversible).

### 2.9 Channels

Transports between the operator and the instance: Telegram (first), the local admin CLI, and later a web UI. A channel authenticates the human, maps chats to sessions, renders events and collects approvals. Channels never execute tools.

### 2.10 Tasks

Units of work tracked by Ka Agent with a persisted state machine: a user request, a reminder, or a scheduled job. A task may run one or more prompt cycles. A task's side effects go through intents (§7.3).

### 2.11 Approvals

A recorded human decision on a specific pending action. There are two kinds:

- **Ka asks**: permission prompts raised by the engine (`crates/ka-engine/src/voice.rs:2786-2823`).
- **Intents**: side-effecting actions proposed through Ka Agent's own plugins.

An approval has an id, an exact subject, an expiry, a decision, and an audit entry.

### 2.12 Long-running work

Work that outlives one prompt cycle or a wall-clock budget (default 10 minutes, provisional). It runs as a task with its own session and worker, progress reporting, cancellation, and an explicit restart policy.

Ka's engine-local background mechanisms do not provide durability for long-running work, and Ka Agent does not rely on them for it:

- Auto-backgrounded bash jobs (`crates/ka-engine/src/hands/jobs.rs:1-9`).
- Background delegate tasks, which are in memory and never revived (`crates/ka-engine/src/hands/tasks.rs:1-4`, `crates/ka-engine/src/hands/tasks.rs:54`).

### 2.13 Identity and user profile

- **Identity** answers "who is the operator". It binds channel identities to the single operator: a Telegram numeric user id after pairing, and the local OS user for the admin CLI.
- **The user profile** is the stable-memory file describing the operator: name, language, timezone, communication preferences.
- **The persona** (Ba's voice) is a separate file describing how the assistant should behave.

## 3. Architecture

### 3.1 Ka engine as-is (evidence)

**[Requirement]** Ka Agent relies only on the behaviors listed in this subsection, through the port defined in §3.4. Anything not listed is treated as a Ka internal. Where Ka's design docs and its code disagree, the code and Ka's contract tests win (`AGENTS.md:46`).

#### 3.1.1 Engine architecture

| Area | What Ka does today | Evidence |
|---|---|---|
| Turn loop | One command loop per engine. A `prompt` starts a *prompt cycle*: one or more turns (model step → permission gate → tool calls, repeated up to `max_steps`, default 20), then optional queued input, auto-poke and verify rounds. The cycle ends with `idle`. | `crates/ka-engine/src/engine.rs:1300-1304`, `crates/ka-engine/src/config.rs:612-615`, `crates/ka-protocol/src/lib.rs:681-683` |
| Sessions / strands | One engine is attached to one strand, chosen by `StrandChoice::{New, Latest, Path}`; `Latest` is per working directory. Attaching recovers dangling turns and restores model, mode and effort **when the strand has `Change` records** — appended only for mid-session `set_model`/`set_mode`/`set_effort` and overflow promotion (`crates/ka-engine/src/engine.rs:1639`, `:1661`, `:1676`, `:2930`), never for the initial config, so a config-sourced model is lost on resume (probe V-10). It then loads history and emits `session_info` and `replay`. | `crates/ka-engine/src/engine.rs:33-40`, `crates/ka-engine/src/engine.rs:145-163`, `crates/ka-engine/src/engine.rs:2381-2539` |
| Persistence | The strand is appended after the turn returns, not per step; append errors are ignored. The writer flushes without fsync and has no locking. A fresh strand file appears only on the first append. | `crates/ka-engine/src/engine.rs:2908-2917`, `crates/ka-engine/src/engine.rs:3246-3264`, `crates/ka-strand/src/lib.rs:590-604`, `crates/ka-strand/src/lib.rs:425-440` |
| Tools (Hands) | Base set: read, edit, write, bash, glob, grep, pathfinder, todo, jobs. Optional: delegate/tasks, remember, web, LSP, debug, MCP. Clearance tiers are read, write and exec. Bash defaults to a 120 s timeout (max 3600 s) with output caps. | `crates/ka-engine/src/hands/mod.rs:38-44`, `crates/ka-engine/src/hands/mod.rs:454-470`, `crates/ka-engine/src/hands/bash.rs:14-23` |
| Permissions | Four modes: guarded, accept_edits, free, plan. `[[rules]]` are data (first match wins). Hardstops ask even in free mode. An unset mode means **free**. | `crates/ka-protocol/src/lib.rs:36-51`, `crates/ka-engine/src/config.rs:23-36`, `crates/ka-engine/src/hands/bashp.rs:304`, `crates/ka-engine/src/config.rs:676-679` |
| Asks | The engine emits `ask` with allow/always/deny and blocks until `answer` or `abort`, with no timeout; other commands are dropped meanwhile. "Always" persists a per-tool-name allow into the project `.ka/ka.toml`. | `crates/ka-engine/src/voice.rs:2786-2823`, `crates/ka-engine/src/voice.rs:2697-2712`, `crates/ka-engine/src/config.rs:711-714` |
| Providers | Three wires behind a `Speaker` trait, an embedded catalog with overlays, and Ollama/LM Studio discovery. Keys are resolved from env, `.env` files, the OS keyring, or a `!command`. | `crates/ka-dialect/src/lib.rs:26-35`, `crates/ka-dialect/src/auth.rs:99-117`, `crates/ka-dialect/src/discovery.rs:260-280` |
| Retries | The wire retries status, connect and first-byte failures. The engine retries retryable failures at 5 s, 20 s and 60 s, then emits the error. | `crates/ka-dialect/src/client.rs:96-158`, `crates/ka-engine/src/voice.rs:3552-3572` |
| Configuration | Strict TOML (unknown keys are errors). `Config::parse_layer` and `Config::overlay` are public. The layer chain (user → trusted project → `--config` → env → flags) is assembled by the binary, not the engine. | `crates/ka-engine/src/config.rs:418-422`, `crates/ka-engine/src/config.rs:493-505`, `crates/ka-harness/src/main.rs:418-456` |
| Errors | `ErrorClass` is auth, rate_limit, overflow, network, protocol, unsupported or internal. A terminal failure emits `error` (`retryable: false`) and then `turn_finished{stop: error}`. | `crates/ka-protocol/src/lib.rs:211-229`, `crates/ka-engine/src/voice.rs:2092-2100`, `crates/ka-protocol/src/lib.rs:162-174` |
| Conventions and memory | `AGENTS.md` is loaded root→cwd. `MEMORY.md` comes from the project root and from `$HOME/.config/ka/MEMORY.md`. `remember` only stages notes in `.ka/memory/inbox.md`. | `crates/ka-engine/src/conventions.rs:101-109`, `crates/ka-engine/src/conventions.rs:46-79`, `crates/ka-engine/src/hands/memory.rs:1-5` |
| Safety extras | The fs sandbox wraps bash only and fails closed. Secret redaction is name-gated and applies to tool output. A trust store gates project `.ka/` content. | `crates/ka-sandbox/src/lib.rs:431-432`, `crates/ka-engine/src/hands/secrets.rs:1-3`, `crates/ka-engine/src/trust.rs:1-6` |

#### 3.1.2 Interfaces

| Surface | Shape today | Evidence |
|---|---|---|
| Wire protocol | `Command`/`Event` serde enums tagged by `type`, with NDJSON helpers. There is no version field, and the enums are not `#[non_exhaustive]`. | `crates/ka-protocol/src/lib.rs:264-474`, `crates/ka-protocol/src/lib.rs:557-560`, `crates/ka-protocol/src/lib.rs:771-781` |
| Streaming | `delta` (text, thought, call), `call_started`, `call_output`, `call_finished`, `context_meter`, `turn_finished` with usage and cost. | `crates/ka-protocol/src/lib.rs:231-246`, `crates/ka-protocol/src/lib.rs:596-653`, `crates/ka-protocol/src/lib.rs:176-189` |
| Resume | Every surface resumes by strand id or prefix: `--session`, serve `resume`, ACP `session/load`, or `StrandChoice::Path`. History arrives as one `replay` event. | `crates/ka-protocol/src/lib.rs:561-566`, `crates/ka-protocol/src/lib.rs:492-510`, `crates/ka-harness/src/serve.rs:431-451` |
| TUI | Runs the engine in-process over the same channels. It is the only surface that uses the full `Command` set (asks, model and mode pickers), and `/memory` inbox acceptance exists only there. | `crates/ka-harness/src/main.rs:1317-1318`, `crates/ka-term/src/tui.rs:3501-3505` |
| `ka run` | One headless prompt cycle. Uses the full config chain. `--print text\|ndjson\|stream-json` (default `text`). Asks are auto-denied. Exit codes: 0 done, 1 error, 2 aborted. | `crates/ka-harness/src/main.rs:73-124`, `crates/ka-harness/src/main.rs:953-963`, `crates/ka-harness/src/main.rs:999-1003` |
| `ka serve` | Hand-rolled HTTP/1.1 with routes `/health`, `/sessions`, `/sessions/{id}`, `/sessions/{id}/prompt` and `/sessions/{id}/events` (SSE with `id:` sequence numbers, a 512-event ring, `Last-Event-ID`, presence). Bearer token; loopback-only without one; 1 MB and 15 s request caps. | `crates/ka-harness/src/serve.rs:1-24`, `crates/ka-harness/src/serve.rs:241-277`, `crates/ka-harness/src/serve.rs:86-108`, `crates/ka-harness/src/serve.rs:148-221` |
| `ka attach` | Observe-only SSE client of a serve session. | `crates/ka-harness/src/main.rs:225-235` |
| `ka acp` | JSON-RPC over stdio: `initialize`, `session/new`, `session/load` (replay forwarded as message chunks within a 2 s drain), `session/prompt`, `session/cancel`, `session/request_permission`. | `crates/ka-harness/src/acp.rs:100-121`, `crates/ka-harness/src/acp.rs:201-219`, `crates/ka-harness/src/acp.rs:425-456`, `crates/ka-harness/src/acp.rs:288-306` |
| Batch | `ka sessions --json` (per working directory), `ka export` (markdown or HTML). | `crates/ka-harness/src/main.rs:242-246`, `crates/ka-harness/src/main.rs:184-188` |
| Crate API | `spawn`, `spawn_with`, `spawn_full` and `spawn_with_speaker` (a scripted speaker, no network). `EngineHandle` is a command sender (capacity 64) plus an event receiver (capacity 256). | `crates/ka-engine/src/lib.rs:21-25`, `crates/ka-engine/src/engine.rs:87-118`, `crates/ka-engine/src/engine.rs:129-130` |
| Extension | MCP client (stdio or HTTP; tools become `<server>.<tool>` exec-tier hands), hooks, `[[rules]]`, skills, markdown subagents, `[features]` toggles. | `crates/ka-engine/src/mcp.rs:24-45`, `README.md:300`, `README.md:302`, `crates/ka-engine/src/conventions.rs:228-252`, `crates/ka-engine/src/config.rs:38-48`, `crates/ka-engine/src/config.rs:405-416` |

#### 3.1.3 Reusable directly by Ka Agent

1. **The whole turn engine:** model catalog, three wires, local model discovery, retries, compaction (digests). Ka Agent writes no model client.
2. **Strands as the conversation record** for model context: resume, replay, fork, transfer. Ka Agent keeps only an index and channel metadata.
3. **The permission engine:** modes, `[[rules]]` as data, hardstops, protected paths, and `ask` events. Ka Agent renders and answers asks; it does not reimplement gating for Ka tools.
4. **The MCP client as the plugin bridge.** Ka Agent exposes its connectors to engines as one MCP server, so plugins need no Ka change (`README.md:300`).
5. **Skills** (`SKILL.md` discovery plus per-turn `skills` on `prompt`, `crates/ka-protocol/src/lib.rs:265-289`).
6. **Convention files** for memory and persona injection (`MEMORY.md`, `AGENTS.md`).
7. **`[guards] spend_usd` / `context_pct`**, which raise asks that Ka Agent turns into budget approvals (`crates/ka-engine/src/config.rs:120-129`).
8. **`spawn_with_speaker`** for deterministic, network-free Ka Agent tests (`crates/ka-engine/src/engine.rs:87-118`).
9. **The fs sandbox for bash, tool-output secret redaction, checkpoints and undo.**
10. **`ka export` / strand rendering** for history export, and `ka-index` (SQLite FTS5) as an optional fallback index.

#### 3.1.4 Missing in Ka

Each gap lists evidence and the Ka Agent-side mitigation. **[Requirement]** No mitigation depends on changing Ka.

1. **G1 — serve/ACP sessions ignore all configuration.** They run `Config::default()`: no model, so fresh sessions get canned echo replies; mode free; no `[[mcp]]`, `[[rules]]`, hooks or `[features]`. Evidence: `crates/ka-harness/src/serve.rs:316-318`, `crates/ka-harness/src/acp.rs:117-120`, `crates/ka-engine/src/config.rs:676-679`, `crates/ka-engine/src/canned.rs:5-11`, `research/ka/agent-boundary.md:59-62`. Mitigation: Ka Agent's worker builds `Config` itself (§3.4).
2. **G2 — no control routes over HTTP.** Serve only ever sends `prompt` (text and schema). There is no answer, abort, mode or model change, compaction, or session close. Evidence: `crates/ka-harness/src/serve.rs:241-277`, `crates/ka-harness/src/serve.rs:399-429`. Mitigation: crate-based worker with the full command set.
3. **G3 — asks never expire.** A serve-session ask cannot be answered at all, so the turn is wedged. Evidence: `crates/ka-engine/src/voice.rs:2786-2823`. Mitigation: Ka Agent enforces an approval TTL and answers `deny` on expiry.
4. **G4 — prompts sent during a running cycle are silently dropped** (serve still returns 200). Evidence: `crates/ka-engine/src/voice.rs:2226-2232`, `crates/ka-engine/src/voice.rs:1835-1839`, `crates/ka-harness/src/serve.rs:411-428`. Mitigation: one prompt in flight per session; a durable Ka Agent queue drained only after `idle`.
5. **G5 — serve session ids are process-local counters.** Sessions are never evicted, and the strand id is visible only in the `session_info` event. Evidence: `crates/ka-harness/src/serve.rs:37-45`, `crates/ka-harness/src/serve.rs:123-138`, `crates/ka-protocol/src/lib.rs:580-585`. Mitigation: Ka Agent owns session ids, records the strand id at bootstrap, and stops idle workers.
6. **G6 — the replay "pinning" in the serve ring checks the first ring entry, but `session_info` is emitted before `replay`.** Long sessions evict the startup history from the backlog. **Confirmed at runtime**: after 95 canned turns, a `Last-Event-ID: 0` reconnect returned 321 backlog events with no `replay` and no `session_info`; no serve test covers the pinned-replay path. Evidence: `crates/ka-harness/src/serve.rs:349-363`, `crates/ka-engine/src/engine.rs:2454-2457`, `crates/ka-engine/src/engine.rs:2537`, probe V-15 in `docs/ka-interface-validation.md`. Mitigation: never use an event backlog as history; read the strand or Ka Agent's own log.
7. **G7 — no protocol version or handshake; closed enums; compatibility by additive convention.** Crates are pre-1.0 and change in lockstep. Evidence: `crates/ka-protocol/src/lib.rs:557-560`, `research/ka/sdk.md:119-123`. Mitigation: exact version pins, lenient decoding of unknown event types at the port, and contract tests per Ka upgrade.
8. **G8 — persistence happens per prompt cycle.** A crash mid-turn loses the user message and every tool step of that turn. Append errors are ignored; there is no fsync and no locking. Evidence: `crates/ka-engine/src/engine.rs:2908-2917`, `crates/ka-engine/src/engine.rs:3246-3264`, `crates/ka-strand/src/lib.rs:590-604`. Mitigation: Ka Agent writes the inbound message and turn intent to SQLite *before* submitting, and guarantees a single engine per strand.
9. **G9 — an interrupted turn is recovered only as a synthetic "(turn interrupted)" message** plus a note; there is no machine-readable stop. Evidence: `crates/ka-strand/src/lib.rs:509-526`, `crates/ka-engine/src/engine.rs:2393-2414`. Mitigation: Ka Agent's own turn table is authoritative for interruption status.
10. **G10 — no scheduler, timers or wake-ups.** Schedules are explicitly out of Ka's scope. Evidence: `research/ka/roadmap.md:228`, `research/ka/agent-boundary.md:90-103`. Mitigation: Ka Agent scheduler (Phase 3).
11. **G11 — no long-term memory beyond strand, digest and `MEMORY.md`.** `remember` only stages notes, and acceptance is a TUI action. Evidence: `crates/ka-engine/src/hands/memory.rs:1-5`, `crates/ka-engine/src/conventions.rs:46-79`, `README.md:245`. Mitigation: Ka Agent's memory tiers (§4).
12. **G12 — "always allow" is coarse.** It is per tool name and written into the project `.ka/ka.toml`. Evidence: `crates/ka-engine/src/voice.rs:2697-2712`, `crates/ka-engine/src/config.rs:711-714`. Mitigation: remote channels never send "always"; durable grants live in Ka Agent's own rules layer.
13. **G13 — engine death is silent:** an `eprintln` and a closed channel. Evidence: `crates/ka-engine/src/engine.rs:129-136`. Mitigation: the worker treats a channel close without `idle` as a crash and reports it.
14. **G14 — engine-side queue, steers, delegate tasks and checkpoint ids live in memory only.** Steering has no consumer on the live-model path: `steers` is only ever pushed, and the sole drain is the canned path — confirmed by exhaustive source check; the existing steer test asserts delivery, not consumption. Evidence: `crates/ka-engine/src/engine.rs:369-376`, `crates/ka-engine/src/engine.rs:3450-3453`, `crates/ka-engine/src/voice.rs:1815`, `crates/ka-engine/src/hands/tasks.rs:54`. Mitigation: Ka Agent never relies on `steer`/`queue`; its queue is durable.
15. **G15 — bash commands running past 30 s (the default threshold) detach and survive the engine by design.** They escape cancellation. Evidence: `crates/ka-engine/src/hands/jobs.rs:1-9`, `crates/ka-engine/src/config.rs:617-618`. Mitigation: the MVP engine layer sets `[tools.bash] background_after_ms = 0`, and each worker runs in its own cgroup.
16. **G16 — strand lookup and listing are per working directory.** Evidence: `crates/ka-engine/src/engine.rs:145-163`, `crates/ka-strand/src/lib.rs:305-307`. Mitigation: Ka Agent stores strand id *and* path per session.
17. **G17 — the sandbox covers bash only** (not MCP servers, hooks or verify commands) and is off by default. Evidence: `crates/ka-sandbox/src/lib.rs:1-22`, `crates/ka-sandbox/src/lib.rs:431-432`. Mitigation: OS-level confinement of the whole worker (§6.4).
18. **G18 — Ka writes keys only to a plaintext `.env`.** Redaction covers tool output only, by env-var name. Cost is 0 for unpriced catalog rows. Evidence: `crates/ka-engine/src/config.rs:759-762`, `crates/ka-engine/src/hands/secrets.rs:1-3`, `README.md:306`. Mitigation: Ka Agent's secret store injects env vars; budgets count tokens as well as USD.
19. **G19 — ACP forwards only the first question of an ask** and drops usage, title and idle events. Evidence: `crates/ka-harness/src/acp.rs:425-456`, `crates/ka-harness/src/acp.rs:479-484`. Mitigation: ACP is not used.
20. **G20 — `ka run` auto-denies every ask.** Evidence: `crates/ka-harness/src/main.rs:953-963`. Mitigation: `ka run` is used only for unattended, fail-closed jobs (§3.4).

#### 3.1.5 Coupling risks and decisions to avoid

- **No Ka Agent code in the Ka workspace.** Ka's root manifest would technically allow it (`Cargo.toml:4-17`), but this would blur release cycles and CI. Ka Agent is a separate repository (`research/ka/agent-boundary.md:90-103`).
- **Never write, or hand-parse, strand files.** `Record` is a closed enum, and one unknown or malformed record mid-file makes the strand unopenable (`crates/ka-strand/src/lib.rs:61-64`, `crates/ka-strand/src/lib.rs:952-967`). Ka Agent reads strands only through `ka_strand` functions, read-only.
- **Never forward local-only commands from remote channels.** These are `shell`, `set_feature` and `set_sandbox` (`crates/ka-protocol/src/lib.rs:316-327`, `crates/ka-protocol/src/lib.rs:355-379`), plus the config-writing `save_api_key` and `save_settings` (`crates/ka-protocol/src/lib.rs:428-464`).
- **Never rely on `Config::default()`.** It means free mode with no model. Every engine gets an explicit mode and model.
- **Never share the operator's interactive Ka state.** That covers strands, waypoints, trust store, spills, skills and memory. Workers run with their own `KA_DATA_DIR`, `XDG_STATE_HOME` and `HOME` (`README.md:312`, `crates/ka-strand/src/lib.rs:244-260`), and `XDG_CONFIG_HOME` must also be pinned or unset: Ka's user config layer resolves through it (`crates/ka-engine/src/config.rs:789-793`), so an inherited value would expose the operator's `ka.toml` (MEMORY.md, skills, rules and the auth `.env` scan are strictly `$HOME`-based, `crates/ka-engine/src/conventions.rs:70-79`, `crates/ka-dialect/src/auth.rs:57-62`).
- **Never trust design docs over code.** For example, `research/ka/sdk.md:72-74` describes a single-consumer stream that ends at `idle`; the code allows many observers and keeps the stream open (`crates/ka-harness/src/serve.rs:1-20`).
- **Never let Ka types leak beyond the port crate.** Swapping boundary implementations must stay a one-crate change.
- **Never make a Ka Agent milestone depend on a Ka change.** Upstream improvements are adopted opportunistically.
- **Avoid the crates.io name `ka-agent` until the legacy-crate question is settled.** It still holds Ka's old binary package (≤0.2.11) (`research/ka/agent-boundary.md:64-88`).

### 3.2 System overview

**[Requirement]** The layering below is fixed; every arrow is a process or crate boundary that can be tested on its own.

```
Telegram / Web / CLI / APIs
        |
   Ka Agent runtime
        |
  Memory + task scheduler + plugin host
        |
   Stable Ka interface
        |
        Ka engine
```

Concretely, in the MVP:

```
Telegram Bot API ──long poll──┐        local admin CLI ──unix socket──┐
                              ▼                                       ▼
                    ┌──────────────── ka-agent daemon (Akh) ────────────────┐
                    │ channels: telegram adapter, admin socket              │
                    │ runtime: session manager, durable queue, approvals,   │
                    │          worker supervisor, budgets, audit            │
                    │ memory (Markdown) · state + FTS (SQLite) · plugin     │
                    │ host = MCP bridge server · scheduler (Phase 3)        │
                    │ KaPort (the only code that knows Ka types)            │
                    └───────┬───────────────────────────────────────────────┘
                            │ stdio NDJSON (ka-protocol Command/Event)
                 ┌──────────▼──────────┐   one worker per active session
                 │ ka-agent __worker    │   own cgroup, own HOME/KA_DATA_DIR
                 │  ka_engine::spawn_full│──► model providers (via ka-dialect)
                 │  (pinned Ka crates)  │──► MCP: ka-agent bridge (stdio)
                 └──────────────────────┘
```

### 3.3 Why Ka Agent is an independent service

**[Requirement]** Ka Agent is its own repository, binary, process and data directory. It is never a Ka feature, plugin or cargo feature.

- **Ka's contract forbids it in-tree.** Ka's footprint contract (≤10 MB, cold start ≤50 ms, idle RSS ≤15 MB) and its dependency allowlist exclude messaging clients, schedulers, plugin runtimes and memory stores (`AGENTS.md:37`, `AGENTS.md:41`, `crates/xtask/src/main.rs:323-327`).
- **The lifecycles are opposite.** Ka is "no daemons" and process-per-session (`research/ka/architecture.md:9`); Ka Agent is the daemon.
- **The failure domains differ.** A wedged engine or provider outage must not take channels, memory or the scheduler down; separate worker processes contain it.
- **The release cycles differ.** Ka Agent upgrades Ka by bumping one pin and re-running contract tests (§3.4).

### 3.4 Boundary choice: `ka serve` vs versioned library (and the others)

**[Provisional]** (confirmed or revised by the Phase 0 spike) For the MVP, the stable Ka interface is a Ka Agent-owned worker process (`ka-agent __worker`). It links the published Ka crates (`ka-engine`, `ka-protocol`, `ka-dialect`, `ka-strand`) pinned to an exact version, and speaks `ka_protocol` NDJSON over stdio to the daemon.

**[Requirement]** Everything above the port uses Ka Agent's own `KaPort` trait: `open_session`, `submit`, `answer`, `abort`, `close`, plus an event stream. Changing the implementation must not touch the runtime, channels or memory.

How each surface covers the MVP's needs at commit `a56aae2`:

| MVP need | `ka serve` | `ka acp` | `ka run --print ndjson` | Crates in a worker |
|---|---|---|---|---|
| Real model on a new session | no (G1) | no (G1) | yes | yes |
| Answer approvals | no (G2, G3) | first question only (G19) | auto-deny only (G20) | yes (`answer`) |
| Abort a running turn | no (G2) | yes | kill process; turn lost (G8) | yes (`abort`) |
| Resume by strand id | yes (server cwd) | yes | yes | yes (`StrandChoice::Path`) |
| Config layers, MCP, rules | no | no | yes (user chain) | yes (Ka Agent composes) |
| Images in a prompt | no (text only) | not verified | no | yes |
| Process isolation | yes | yes | yes | yes |
| Compile-time coupling | none | none | none | exact version pin |

When to use each:

- **Versioned library (now).**
  - It is the only shipped surface that covers approvals, abort, model and mode selection, and config composition together (`crates/ka-engine/src/config.rs:493-505`, `crates/ka-protocol/src/lib.rs:264-474`).
  - Isolation comes from the worker process, not from the transport.
  - Pins:
    - **[Requirement]** An exact `=` version, upgraded deliberately, with the contract suite green.
    - **[Provisional]** A git revision until 0.3.0 is published.
- **`ka serve` (later, conditional).** It becomes the preferred boundary, with no compile-time coupling and the user's installed `ka` binary, only once a released Ka offers all of the following:
  1. Sessions created with explicit model, mode and config.
  2. Asks answerable and turns abortable over HTTP.
  3. Busy prompts rejected or queued, not dropped.
  4. The strand id returned on create.
  5. Sessions closable.

  Ka Agent does not request or depend on these changes; it checks for them on each Ka release (Phase 0 checklist). Until then, serve is used only in the Phase 0 spike, to re-confirm G1–G6.
- **`ka run --print ndjson`.**
  - **[Provisional]** For unattended, approval-free jobs from Phase 3: read-only digests and RSS summaries.
  - Auto-deny makes it fail closed.
  - **[Requirement]** It is always invoked with an explicit `--mode plan` (or `guarded`) and Ka Agent's `--config` layer, so the operator's personal config (for example `mode = "free"`) can never loosen an unattended run.
- **`ka acp`.** Not used (G1, G19).

**[Provisional]** How the worker composes the engine's configuration:

- Engine defaults, overlaid by Ka Agent's engine layer `~/.config/ka-agent/engine.toml`. This is Ka's own strict format, validated with `Config::parse_layer`, so `ka config schema` documents it.
- Then a security layer that Ka Agent always applies last:
  - `mode = "guarded"`;
  - `[tools.bash] background_after_ms = 0`;
  - `[features] disable = ["tool:remember"]`, because memory proposals go through Ka Agent (§4.4);
  - `[[mcp]]` pointing at the Ka Agent bridge (from Phase 2, when the bridge exists);
  - `[guards] spend_usd` from the budget.
- The operator's `~/.config/ka/ka.toml` is not read by default.

### 3.5 Mapping a Telegram conversation to a Ka session

**[Requirement]** The mapping is explicit, persisted and one-directional: `telegram user id → operator → chat binding → Ka Agent session → strand`.

- `bindings(channel, chat_id) → active_session_id`. MVP: private chats only, one active session per chat.
- `sessions(id, workspace, strand_id, strand_path, status, …)`. The id is a ULID owned by Ka Agent and never reuses Ka ids.
- **First message or `/new`:**
  1. Spawn a worker with `StrandChoice::New` in the workspace.
  2. Read bootstrap events up to `replay`. The strand id comes from `session_info` (`crates/ka-protocol/src/lib.rs:580-585`).
  3. Persist the strand id.
  4. Submit the prompt.
- **[Requirement]** The strand file appears only after the first completed turn (`crates/ka-strand/src/lib.rs:425-440`). A session with a recorded strand id but no file is treated as empty and reopened with `StrandChoice::New`.
- `/sessions` lists recent sessions — **not implemented** (the operator
  inspects `ka-agent status` / the web history view instead); `/resume`
  lifts a paused session back to idle (implemented in M10; rebinding to an
  arbitrary past session is not).

### 3.6 Resuming a session after a restart

**[Requirement]** After any restart (daemon, worker or machine), resumption is lazy, explicit and policy-reasserting.

1. Nothing is spawned at boot. The first inbound message for a binding starts the worker with `StrandChoice::Path(strand_path)`.
2. The engine recovers a dangling turn (note "↩ recovered interrupted turn") and restores model, mode and effort from the strand **only when the strand recorded `Change` entries**. The initial config is never recorded, so a resumed strand can come back with no model at all (`crates/ka-engine/src/engine.rs:2393-2426`, probe V-10).
3. **[Requirement]** Ka Agent therefore always re-asserts its policy before submitting: it sends `set_mode` and `set_model` — which also persist as `Change` records, making the next resume self-restoring — so a strand recorded in a looser mode cannot loosen policy and a resumed session is never left model-less.
4. Turns that were in flight when the process died are already marked `interrupted` in Ka Agent's turn table (§7.1). The user is told what was interrupted, and **[Requirement]** nothing is re-submitted automatically.

### 3.7 Replayed history vs new responses

**[Requirement]** Only events caused by a Ka Agent submission are ever delivered to a channel as new output.

- With the crate worker, history arrives as exactly one `replay` event per attach (`crates/ka-protocol/src/lib.rs:561-566`), at startup and after strand switches. Ka Agent never forwards `replay` to Telegram, which already shows that history.
  - **[Provisional]** Ka Agent uses `replay` only to check continuity: does the last assistant message match its own log?
- New output is the span from Ka Agent's `prompt` to the next `idle`.
  - One prompt cycle may contain several `turn_started` … `turn_finished` pairs (auto-poke, verify, queued input), so `idle` is the delivery boundary (`crates/ka-protocol/src/lib.rs:681-683`).
  - Ka Agent tags the cycle with its own `turn_id`. This is valid because exactly one prompt is in flight per session.
- Outbound messages are written to an outbox keyed `(turn_id, chunk_index)`, so a crash during delivery cannot produce out-of-order or duplicated chunks beyond the single chunk being sent.
- A future `ka serve` port would deduplicate by SSE `id:` with a stored high-water mark per serve session. It would also re-create sessions on serve restart, because ids restart at `s1` (G5).

### 3.8 Errors, cancellations and restarts

**[Requirement]** Every failure ends in a visible, recorded state; nothing that may have had side effects is retried automatically.

- **Model and provider errors.** Ka retries internally (wire level, then 5 s / 20 s / 60 s, then fallback models). Ka Agent receives `error{class}` then `turn_finished{stop: error}` and reacts by class:

| `ErrorClass` | Ka Agent reaction |
|---|---|
| `auth` | Pause the instance; alert the operator via Telegram and the CLI. |
| `rate_limit` | Report it; offer a **Retry** button (no auto-retry). |
| `network` | Report it; offer a **Retry** button (no auto-retry). |
| `overflow` | Offer `/compact` (Ka's `compact`) or `/new`. |
| `protocol`, `unsupported`, `internal` | Report a short summary; log details. |

- **Cancellation.** `/stop` or the **Stop** button sends `abort`. Ka Agent expects `turn_finished{stop: aborted}` and then `idle` within 10 s **[Provisional]**. Otherwise it sends SIGTERM, then after 5 s SIGKILL, to the worker's cgroup, which also kills tool children. The turn is marked `aborted` or `killed`.
- **Worker crash** (channel closed without `idle`, G13):
  1. Mark the cycle `interrupted` and notify the user.
  2. Respawn on the next message with backoff from 1 s up to 60 s.
  3. After 5 crashes in 10 minutes **[Provisional]**, mark the session `degraded`; resuming it then needs `/resume` or `/new`.
- **Daemon restart.**
  1. A graceful stop stops polling, waits up to 60 s for prompt cycles to reach `idle`, aborts the rest, flushes state and exits.
  2. A crash restart reconciles: `running` becomes `interrupted`, `awaiting_approval` for Ka asks becomes `expired` (the engine is gone), and queued messages stay queued.

### 3.9 State ownership: Ka vs Ka Agent

**[Requirement]** Each piece of state has exactly one owner and one writer.

| State | Owner, writer | Ka Agent access |
|---|---|---|
| Strands (model-visible history, digests, settings) | Ka engine | Read via `ka_strand`; never written |
| Spills, detached jobs, trust store, waypoints | Ka engine, inside the worker's isolated dirs | None, apart from cleanup on session deletion |
| Sessions, bindings, turn table, queue, outbox | Ka Agent (SQLite) | Read/write |
| Approvals, intents, tasks, runs, audit | Ka Agent (SQLite) | Read/write |
| Stable memory, profile, persona | Ka Agent (Markdown) | Read/write after acceptance |
| Compiled `MEMORY.md`, provisioned skills in the engine home | Ka Agent writes; engine reads | Regenerated, never hand-edited |
| Episodic index (FTS) | Ka Agent (SQLite) | Rebuildable from the turn table and strands |
| Secrets | Ka Agent secret store | Injected into the worker env per session |

## 4. Memory model

### 4.1 Three tiers

**[Requirement]** Memory is split into three tiers with different write rules:

- **Stable memory**
  - Content: preferences, projects, environment, conventions, durable facts, the profile and the persona.
  - Writes: small, human-approved, versioned.
  - Reads: on every session, budgeted.
- **Episodic memory**
  - Content: the history of sessions and their outcomes (prompt cycles, final answers, errors, costs, links to strands).
  - Writes: append-only, automatic.
  - Reads: on demand, through search.
- **Operational state**
  - Content: tasks, reminders, approvals, intents, queues and in-flight jobs.
  - Writes: transactional state machines.
  - Reads: by the runtime.

### 4.2 Storage format

**[Provisional]** Markdown for stable memory; SQLite (WAL mode) for operational state and indexes:

```
~/.local/share/ka-agent/
  memory/                 # git repository, auto-committed on every accepted change
    profile.md  persona.md  preferences.md  environment.md  conventions.md
    projects/<slug>.md    facts.md
  state.db                # SQLite: sessions, bindings, turns, queue, outbox, approvals,
                          # intents, tasks, runs, memory_candidates, audit, episodes_fts
  ka/                     # KA_DATA_DIR for workers (strands)
  engine-home/            # HOME for workers: .config/ka/MEMORY.md, .config/ka/skills/
  engine-state/           # XDG_STATE_HOME for workers (spills, trust, jobs)
  attachments/<yyyy-mm>/  backups/  logs/
```

**[Requirement]** Stable-memory entries carry provenance as an HTML comment: `<!-- id: m_…; source: session/turn or "operator"; accepted: date; valid_until: optional -->`. This keeps the files readable and diffable while making every fact traceable.

### 4.3 When to introduce semantic search or embeddings

**[Provisional]** Not in the MVP. Embeddings are reconsidered at the Phase 2 exit and adopted only when all three conditions hold:

1. A labeled set of at least 50 real operator queries shows FTS5 recall@10 below 0.8.
2. The episodic corpus exceeds about 5,000 prompt cycles, or stable memory exceeds the injection budget so that retrieval must select.
3. A local embedding model is available, so that no history leaves the machine by default.

If adopted, vectors live in SQLite next to the FTS index, are rebuildable, and are never a source of truth.

### 4.4 Updating stable memory without conversational noise

**[Requirement]** No stable-memory write happens without an explicit operator act. Writes arrive through three paths:

- An explicit `/remember <fact>` from the operator.
- Acceptance of a *candidate* in `/memory review` (from Phase 2).
- Direct editing (`ka-agent memory edit`), which opens `$EDITOR`.

How candidates are produced and kept clean:

- From Phase 2, candidates come from the model through Ka Agent's `memory.propose` tool, exposed by the MCP bridge. Ka's own `remember` hand is disabled (§3.4), so proposals carry session and turn provenance and never touch the workspace.
- **[Provisional]** Candidate rules: an atomic statement of at most 300 characters, deduplicated against existing entries, rejected if it contains secrets (redaction patterns plus provider key formats), and optionally carrying `valid_until` for volatile facts. Unreviewed candidates expire after 14 days.
- Episodic text is never promoted wholesale.

Injection into engines:

- **[Provisional]** Ka Agent compiles stable memory into `engine-home/.config/ka/MEMORY.md`, the user tier that Ka loads from `$HOME` (`crates/ka-engine/src/conventions.rs:46-79`).
- The compiled file has a 6 KB budget: profile, preferences, active projects, environment, conventions. From Phase 2, the rest is reachable through a read-only `memory.search` tool.
- The persona and operating rules go into the same compiled file.
- Verified 2026-10-15: Ka re-reads `MEMORY.md` from disk on every prompt, inside the per-prompt system-prompt rebuild (`crates/ka-engine/src/voice.rs:1634-1639`, called from `crates/ka-engine/src/engine.rs:2879-2889`). A Ka Agent memory update takes effect on the worker's next prompt; no worker restart is needed.

### 4.5 Searching history without loading full sessions

**[Requirement]** History search never loads strands wholesale. Search hits the index; full detail is fetched only for a selected hit.

- **[Provisional]** `episodes_fts` (SQLite FTS5) indexes per prompt cycle: user text, final assistant text, tool names used, outcome, plus later summaries. Each row is keyed by `(session_id, turn_id)` with time, chat and cost columns.
- `/history <query>` and the `history.search` tool return snippets with ids. `/history show <id>` reads exactly that cycle, from Ka Agent's log first and the strand (read-only, via `ka_strand::read`) for tool detail.
- **[Provisional]** Episode summaries (Phase 2) are produced by a separate `ka run --mode plan` job on a cheap model after a session has been idle for 2 hours. They are stored in SQLite and never written into strands.

### 4.6 Retention, deletion, export and user control

**[Requirement]** The operator can see, export and delete everything Ka Agent stores, with documented commands:

- **Retention.**
  - **[Provisional]** Defaults: stable memory and episodes are kept until deleted. Raw channel payloads and attachments are kept for 90 days. Event debug logs are kept for 30 days. Backups keep 7 daily and 4 weekly copies.
  - Strands follow their session's lifetime.
- **Deletion.**
  - `/forget <memory-id>` and `ka-agent memory rm` delete a memory entry (a git commit keeps local history until the history is pruned).
  - `ka-agent history rm <session>` deletes the index rows, the attachments, the strand file in Ka Agent's isolated data dir, and the session's event logs.
  - `ka-agent purge --all` wipes the instance.
- **Export.** `ka-agent export <dir>` writes:
  - the memory Markdown;
  - a JSONL dump of every state table;
  - strands, also rendered to Markdown via the strand renderer;
  - a manifest with checksums.
- **Control.**
  - Memory is plain files.
  - `/memory` shows what is injected and its size against the budget.
  - Injection can be paused per session (`/memory off`).

## 5. Channels and interfaces

### 5.1 Channel plan: Telegram first, CLI for administration, web later

**[Requirement]** The channel surfaces have fixed roles:

- **Telegram is the only conversational channel in the MVP.**
- **The local admin CLI (`ka-agent …`) is the only place where privileged changes happen:** pairing, grants, budgets, policy, plugin enablement and secrets. It talks to the daemon over a Unix socket (`0600`, peer-credential check).
- **A web UI is a Phase 4 goal, not an MVP requirement.**

**[Provisional]** The Telegram transport is Bot API long polling (`getUpdates`), so there is no inbound port and no webhook TLS. It is a small hand-written client covering about 8 methods; no bot framework.

### 5.2 Authentication and allowlist

**[Requirement]** Only the paired operator's numeric Telegram user id is served. Usernames are mutable, so they are never trusted.

- **Pairing:**
  1. `ka-agent pair telegram` prints a one-time 8-character code with a 10-minute TTL.
  2. The operator sends `/pair <code>` to the bot in a private chat.
  3. Ka Agent binds `from.id`, records an audit entry, and invalidates the code.
- **[Requirement]** Messages from any other user, or from non-private chats, get no reply. They are rate-limited in the log and audited as `unauthorized_contact`.
- **[Requirement]** Every callback query (button press) checks `from.id` against the paired operator, not only the chat.
- **[Requirement]** The bot token lives in Ka Agent's secret store (§8.5), never in config files committed to git.

### 5.3 Mapping `chat_id`, user and session

**[Requirement]** One operator can have several bindings, but each binding has at most one active session.

- `channel_identities(channel="telegram", external_user_id) → operator` (single row in the MVP).
- `bindings(channel, chat_id) → active_session_id`. MVP: one private chat.
- Commands: `/new` (fresh session), `/sessions`, `/resume <id>`, `/name <word>` (sent as Ka's `set_name`: one word, at most 64 characters), `/status`.
- **[Provisional]** Inbound `update_id`s are stored before processing, with a unique constraint, and the polling offset advances only after the row commits. This gives at-least-once ingestion with idempotent processing.

### 5.4 Long messages, files, commands and multi-message replies

**[Requirement]** The limits below were verified against the official Bot API pages on 2026-10-15: 4096 characters per text message, 1–64 byte `callback_data`, 20 MB bot downloads, 50 MB uploads, ~5 s typing status, and `getUpdates` long polling with offset confirmation (an update is confirmed as soon as a higher offset is requested). The ~30 messages/second figure is officially framed as a bulk-broadcast cap; per-chat guidance is about 1 message/second (groups: 20/minute).

- **Progress.** While a cycle runs: `sendChatAction typing`, plus one editable progress message showing tool activity (`call_started` details). Edits are at most one every 3 s — no official per-second cap exists for `editMessageText`, so this is our own conservative pacing, and 429 responses are retried with backoff.
- **Final answer.**
  - Rendered from Markdown to Telegram HTML.
  - Split at paragraph boundaries under 3,800 characters; code fences are closed and reopened across chunks.
  - Above 6 chunks it is sent as one `.md` document plus a short summary.
  - Each chunk goes through the outbox (§3.7).
- **Inbound files.**
  - Downloaded to `workspace/inbox/<date>/` (quarantine, size-capped).
  - The prompt mentions the path.
  - Images under 5 MB are also attached as `images` on `prompt` (`crates/ka-protocol/src/lib.rs:265-289`).
- **Outbound files.** **[Provisional]** In the MVP, the only outbound files are long answers sent as `.md` documents. From Phase 2, files go out only through the `telegram.send_file` tool, restricted to the workspace (§6.6).
- **Commands.**
  - `/start`, `/pair`, `/help`, `/new`, `/sessions`, `/resume`, `/stop`, `/status`.
  - `/remember`, `/memory`, `/forget`, `/history`.
  - `/skills`, `/skill <name> <text>` (sent as `prompt.skills`).
  - `/compact`, `/budget`.
  - Unknown slash commands are answered locally and never sent to the model.

### 5.5 Conversation and approval states

**[Requirement]** These state machines are persisted and drive every channel reply.

| Conversation state | Meaning | An inbound message… |
|---|---|---|
| `idle` | No cycle in flight | …is submitted |
| `running` | A cycle is in flight (`prompt` sent, no `idle` yet) | …is queued durably; reply "queued (n)" |
| `awaiting_approval` | The engine is blocked on an ask | …is queued; the approval card stays pinned |
| `stopping` | Abort sent, waiting for `idle` or kill | …is queued |
| `paused` | Budget exhausted, auth failure, or operator pause | …gets the reason and `/status`; it is not queued |
| `degraded` | Worker crash loop | …gets an explanation; `/resume` or `/new` |

- **Approval states:** `pending → approved | denied | expired | cancelled | superseded`.
- **Approval cards** show:
  - what is being asked (tool and argument summary, plus the diff from `ask.questions[].detail` when present, `crates/ka-protocol/src/lib.rs:248-259`);
  - the session;
  - the expiry;
  - buttons **Allow once**, **Deny** and **Stop turn**.
- **[Requirement]** Telegram never offers "always" (G12).
- **[Provisional]** The approval TTL is 15 minutes. On expiry Ka Agent answers deny (choice 2) and edits the card to "expired".

### 5.6 Security limits for actions with external effects

**[Requirement]** Chat is a convenient but weak authentication factor, so the following hold from a Telegram-initiated session:

- **Policy changes are CLI-only.** Mode changes, grant changes, plugin enablement, secret changes and budget increases are not available from Telegram.
- **Outbound messaging is scoped.** Messages go only to the bound chat; there is no tool to message arbitrary chats or users.
- **Work stays in the workspace.** Workers run with the workspace as cwd and Ka's fs sandbox enabled where available; writes outside it need an approval, and some never happen (§8.4).
- **External-write and irreversible effects need an approval every time** (§8.3), and an exec ask is never auto-approved by Ka Agent.
- **Network is limited.** No network listeners in the MVP; the only outbound traffic is to model providers, Telegram, and hosts declared in enabled plugin manifests.

## 6. Skills, plugins and integrations

### 6.1 Definitions: skills, plugins, connectors, tools

**[Requirement]** The four extension concepts have fixed meanings, and none of them grants authority by being installed:

- **Skill**: a `SKILL.md` directory with instructions or a procedure. Ka Agent provisions enabled skills into `engine-home/.config/ka/skills/`, which Ka discovers (`crates/ka-engine/src/conventions.rs:228-252`). Skills carry no executable authority, and scripts bundled inside a skill run only through Ka's normal gated tools.
- **Plugin**: an installable package with a manifest (§6.2) that provides tools, and optionally skills.
- **Connector**: a plugin bound to an external service and its credentials.
- **Tool**: a single callable action with a JSON schema, an effect class and scopes. Every Ka Agent tool reaches engines through a single MCP server, the **Ka Agent bridge**, configured as the engine's only `[[mcp]]` entry. Ka therefore sees `kagent.<tool>` exec-tier hands (`README.md:300`), and Ka Agent remains the policy enforcement point for each call.

### 6.2 Plugin manifest

**[Provisional]** The manifest format (TOML, strict: unknown keys are errors, mirroring Ka):

```toml
[plugin]
id = "github"                      # unique, [a-z0-9-]
version = "0.1.0"                  # semver of the plugin
api = 1                            # Ka Agent plugin API major
kind = "connector"                 # tool | connector | skill-pack
description = "Issues and pull requests on GitHub"
entry = { type = "builtin" }       # MVP/Phase 3; Phase 5: { type = "mcp-stdio", command = "…", args = [] }

[requires]
secrets = ["github.token"]         # names in Ka Agent's secret store
network = ["api.github.com:443"]   # egress allowlist
filesystem = { read = [], write = [] }

[[tools]]
name = "github.issues.list"
effect = "read"                    # read | local_write | external_write | exec | irreversible
scopes = ["github:issues:read"]

[[tools]]
name = "github.issues.comment"
effect = "external_write"
scopes = ["github:issues:write"]
approval = "always"                # derived default for external_write; cannot be lowered below the effect floor
idempotency = "client_key"         # none | client_key | natural_key
```

### 6.3 Permissions and scopes

**[Requirement]** A tool call proceeds only if all of the following hold:

1. The plugin is enabled.
2. Every scope the tool declares has been granted by the operator through the CLI (`ka-agent grant github:issues:write --repo owner/name`).
3. The call's arguments fall inside the grant's constraints (repo, path, host, calendar id).
4. The effect's approval floor is satisfied.

Grants are stored in SQLite with creator, time and optional expiry, and are listed by `ka-agent grants`.

- **Effect floors:** `read` needs no approval. `local_write` needs no approval inside granted roots. `external_write` needs approval every time in the MVP. `exec` follows Ka's ask flow. `irreversible` needs approval plus a typed confirmation of the target name.
- **[Requirement]** Ka Agent generates one `[[rules]]` allow entry per bridge tool in the engine layer, so Ka never raises its own ask for `kagent.*` calls. Verified 2026-10-15: MCP hands are named `<server>.<tool>`, rules match by exact tool-name equality and are checked before mode logic (`crates/ka-engine/src/mcp.rs:842-861`, `crates/ka-engine/src/voice.rs:3010-3038`), so `tool = "kagent.<tool>"` allow entries work. The bridge is the single approval point, using the effect floors above.

### 6.4 Sandbox and execution limits

**[Requirement]** Isolation is defined per component:

- **Ka workers** run as the operator's user, but each in its own transient systemd scope (cgroup) with memory and CPU limits, a private `HOME`, `KA_DATA_DIR` and `XDG_STATE_HOME` under `~/.local/share/ka-agent/`, and `XDG_CONFIG_HOME` pinned to the engine home (Ka's user config layer reads it, `crates/ka-engine/src/config.rs:789-793`; waypoints live under `KA_DATA_DIR`, `crates/ka-engine/src/engine.rs:3323-3325`). Ka's `[sandbox] mode = "fs"` is enabled when an enforcement tool exists (`crates/ka-sandbox/src/lib.rs:431-432`). It fails closed if configured but unavailable; Ka Agent then reports the failure instead of silently disabling it.
- **MVP plugins are first-party built-ins** compiled into Ka Agent. There is no third-party code execution.
- **[Provisional] Phase 5 third-party plugins** run as MCP stdio subprocesses behind the bridge, under bubblewrap: no network namespace by default; egress only through a local allowlisting proxy derived from `requires.network`; read-only root; a private tmp. Each process has a wall-clock limit per call (30 s default), a memory cap and an output cap.
- **[Requirement]** Plugins never receive the bot token, provider keys or other plugins' secrets. Each process gets only the secrets it declares.

### 6.5 Versioning and compatibility

**[Requirement]** Compatibility is declared and checked at the boundary:

- `api` is a major version; the host supports `api = N` and `N-1`. Tool names are stable; a breaking schema change gets a new tool name.
- Plugin `version` is semver. The manifest is validated at install and at startup. An incompatible plugin is disabled with a visible reason, never half-loaded.
- **[Provisional]** From Phase 5, third-party plugins are distributed as signed archives (ed25519, the same primitive Ka uses for its own updates) and pinned by checksum. Updates are explicit, with no auto-update.
- Ka itself is versioned separately. Ka Agent records the Ka version and its contract-suite hash in `ka-agent doctor` output.

### 6.6 Examples

**[Provisional]** Example first-party plugins and their policies:

- **filesystem**:
  - `fs.read`, `fs.list` (read, granted roots);
  - `fs.write` (local_write inside granted roots, otherwise not offered);
  - `fs.delete` (irreversible).
  - Mostly redundant with Ka's own tools inside the workspace; it exists for extra roots such as `~/notes`.
- **GitHub**:
  - `issues.list` and `pulls.get` (read);
  - `issues.comment` and `pulls.review` (external_write, approval every time, `client_key` idempotency recorded in the intent);
  - token scoped to the granted repos.
- **calendar** (CalDAV or ICS URL):
  - `calendar.agenda` (read);
  - `calendar.create_event` (external_write; the card shows title, time and attendees);
  - `calendar.delete_event` (irreversible).
- **notes**: a Markdown notes directory; `notes.search` and `notes.read` (read); `notes.append` (local_write); git-versioned like memory.
- **RSS**:
  - `rss.fetch` (read, host allowlist from subscriptions);
  - used by Phase 3 scheduled digests through `ka run --mode plan`.
- **HTTP API**:
  - `http.get` (read, granted hosts only);
  - `http.post`, `http.put` and `http.delete` (external_write or irreversible, per grant);
  - responses capped at 200 KB and redacted.
- **Telegram (as a tool)**:
  - `telegram.send_message` and `telegram.send_file`, to the bound chat only (local_write class, no approval);
  - used for long-running work notifications;
  - no tool can address another chat.

## 7. Persistence and reliability

### 7.1 What survives a restart

**[Requirement]** These survive any daemon, worker or machine restart:

- bindings, sessions (with strand id and path), the turn table, queued inbound messages and the outbox;
- approvals and intents, with their states;
- tasks and schedules, and the run history;
- stable memory, episodes and the FTS index, and the audit log;
- strands, which are Ka's;
- attachments.

What does *not* survive, by design or because of Ka limits:

- the in-flight prompt cycle (G8);
- pending Ka asks (the engine is gone);
- engine-local queue, steers, delegate tasks and checkpoint ids (G14);
- progress messages.

On startup, Ka Agent reconciles:

1. `turns.running` becomes `interrupted`.
2. Ka-ask approvals become `expired`.
3. The operator gets one summary message listing what was interrupted.

### 7.2 Recovering pending tasks

**[Requirement]** Tasks are durable state machines: `queued → running → (awaiting_approval) → succeeded | failed | cancelled | interrupted | unknown`.

- **`queued`** tasks resume normally.
- **`running`** tasks become `interrupted`:
  - Read-only kinds (an RSS digest, a history summary) are re-queued automatically once.
  - Anything that may have side effects waits for the operator: **Re-run** or **Discard**.
- **Approvals for intents survive restarts.** The action has not executed yet, so the card is re-sent with the remaining TTL.
- **[Provisional]** Missed reminders and schedules fire late if they are less than 1 hour overdue, marked "late by Xm". Otherwise they are recorded as `missed` and reported.

### 7.3 Never repeating destructive actions after a failure

**[Requirement]** No side-effecting action is ever executed twice because of a crash or retry.

- **Intent log.** Every Ka Agent side-effecting tool call is an intent row:
  1. `proposed` (arguments hash plus idempotency key)
  2. `approved`
  3. `executing`, committed before the external call
  4. `done(external_ref)` or `failed`.

  An intent found in `executing` at startup becomes `unknown`. It is **never retried automatically**. The operator gets the details and a reconciliation hint (for example, "check whether issue comment X exists").
- **Idempotency.** Connectors with `client_key` or `natural_key` idempotency perform a lookup before re-executing an `unknown` intent, and only on operator request.
- **No auto-replay of Ka turns.** Interrupted prompt cycles are never re-submitted automatically, because Ka tools may have run (G8).
- **Retries are allowed only for read effects and for Telegram sends,** which are idempotent per outbox chunk.
- **[Provisional]** Before a cycle in a git workspace, Ka Agent may send Ka's `checkpoint` command, so that the operator can restore after a bad run.

### 7.4 Execution log: runs, results and errors

**[Requirement]** Every unit of execution is recorded with enough detail to explain outcomes without rerunning anything.

- **`runs`:** id, kind (prompt cycle, task, intent, worker), session or task, start and end times, status, `stop`, `ErrorClass`, error message (redacted), tokens, cost and the Ka version.
- **Raw Ka events:**
  - **[Provisional]** Ka events are appended per session to `logs/events/<session>/<date>.ndjson`. This is Ka Agent's copy, used for debugging and audit, and kept 30 days.
  - Prompt text is stored hashed in that log.
- Results stay in the turn table and the outbox. Errors are user-visible summaries plus log details.

### 7.5 Backups and migrations

**[Requirement]** Backups happen automatically and are restorable with one command; schema changes are forward-only and guarded.

- **Backups.**
  - A nightly `VACUUM INTO backups/state-<date>.db`.
  - Memory lives in a git repository and is copied as-is.
  - The strand directory and attachments are copied to `backups/` as a tarball.
  - Rotation keeps 7 daily and 4 weekly copies.
  - `ka-agent backup now` and `ka-agent restore <archive>` (the daemon must be stopped).
  - **The Phase 1 exit requires a tested restore drill.**
- **Migrations.**
  - `PRAGMA user_version` with numbered, embedded SQL migrations that are forward-only and transactional.
  - An automatic backup runs before migrating.
  - **[Requirement]** The daemon refuses to start on a database newer than it understands.
  - Memory Markdown has no schema migrations beyond additive provenance fields.

### 7.6 Observability: structured logs, metrics, traces

**[Requirement]** Operating the instance needs only local tools. No telemetry leaves the machine by default.

- **Structured logs.** JSON lines via `tracing` to the journal or a file. Fields: `instance`, `session_id`, `turn_id`, `run_id`, `worker_pid`, `channel`, `event`. Secrets and message bodies are excluded by default.
- **Metrics.**
  - Exposed through `ka-agent status --json`.
  - **[Provisional]** Optionally a Prometheus text endpoint on loopback.
  - Signals:
    - prompt cycles (total, and failed by `ErrorClass`);
    - cycle latency (p50 and p95);
    - tokens and USD per day;
    - pending and expired approvals;
    - queue depth;
    - worker restarts;
    - Telegram API errors;
    - outbox lag;
    - backup age.
- **Traces.**
  - **[Provisional]** OpenTelemetry export behind a cargo feature, off by default.
  - One span per inbound message, then the prompt cycle, then tool calls built from `call_started`/`call_finished`.

## 8. Security and permissions

### 8.1 User identity

**[Requirement]** Exactly one operator per instance. Only these identities are accepted:

- the local OS user that owns the daemon, for the admin CLI (Unix socket with peer credentials);
- one paired Telegram numeric user id.

Pairing, unpairing and identity changes happen only from the CLI and are audited. Multi-user support is out of scope until Phase 6, and a separate instance per person remains the recommended model.

### 8.2 Scopes per connector and tool

**[Requirement]** Authority is granted per scope with constraints, never per plugin. Scopes are written `<connector>:<resource>:<access>`, for example `github:issues:write`, with constraint fields such as `repo`, `path`, `host` or `calendar`.

- Grants are explicit, listed, revocable and optionally expiring.
- A plugin requesting a new scope after an update stays disabled until the new scope is granted.
- **[Provisional]** Ka's own tools inside workers are governed by Ka's mode plus Ka Agent's rules layer. The default is `guarded` with an allowlist of read-only and test commands.

### 8.3 Explicit approval for irreversible or external-effect actions

**[Requirement]** Each of these requires a per-action approval card that shows the exact target and payload summary:

- `external_write` and `irreversible` effects;
- every Ka exec-tier ask that is not covered by a pre-authored allow rule;
- every Ka ask raised by a hardstop or a protected path.

How approvals are enforced:

- Approvals are single-use, bound to the exact action (arguments hash), expire after the TTL, and deny on expiry.
- Spend-guard asks from Ka (`[guards] spend_usd`) are budget approvals with the same mechanics.
- Approval records are written before the answer is sent to Ka or the intent executes.

### 8.4 Read, write and execute separation

**[Requirement]** Read, write and execute are separate classes everywhere:

- In Ka, the clearance tiers (read, write, exec).
- In Ka Agent, the effect classes (read, local_write, external_write, exec, irreversible).

The workspace is the only default write root, and execution happens only inside workers. No tool ever writes these paths:

- Ka Agent's own configuration, secrets, `state.db` and `memory/`. Memory changes go through the memory API.
- The engine home's `ka.toml`, `.env` and `trust.json`.
- `~/.ssh` and shell rc files. Ka already turns writes to some of these paths into asks, even in free mode (`crates/ka-engine/src/hands/protected.rs:1-9`, `crates/ka-engine/src/hands/protected.rs:51-62`). Ka Agent auto-denies those asks without showing a card, and its sandbox allowlist and rules cover the rest.

### 8.5 Secrets and credentials

**[Requirement]** Secrets live in one store, with names, and are delivered to processes only as needed:

- **[Provisional]** The OS keyring (Secret Service) is the primary store, with a `0600` age-encrypted file as the headless fallback.
- Delivery to workers is by environment variable, only for the providers that session needs. Ka resolves keys from the environment (`crates/ka-dialect/src/auth.rs:99-117`).
- The bot token and connector tokens never enter a worker.
- Secrets are never written into memory, strands, logs or approval cards.
- Ka Agent redacts known secret values (exact-match) in everything it sends to Telegram, in addition to Ka's name-gated redaction (G18).
- `ka-agent secrets set|rm|list` lists names only and never prints values.

### 8.6 Spend, rate and access limits

**[Requirement]** Limits are enforced before work starts, not only reported afterwards:

- **Spend.**
  - A per-session `[guards] spend_usd` in the engine layer (Ka asks to continue when it is crossed, `crates/ka-engine/src/config.rs:120-129`).
  - A Ka Agent daily cap in USD **and** tokens, counted from `turn_finished.usage`, because unpriced models report a cost of 0 (G18).
  - When the cap is reached, the instance is `paused` until midnight local time or a CLI override.
- **Rate.**
  - **[Provisional]** At most 20 inbound messages a minute per chat; excess messages are dropped with one notice.
  - One prompt in flight per session.
  - One active prompt cycle per workspace in the MVP, to avoid concurrent edits.
  - Telegram sends are paced at about 1 per second per chat.
- **Access.** Workspace-only cwd. Network egress for plugins is limited to the hosts their manifests declare. There are no listeners other than the admin socket (and later the loopback web UI).

### 8.7 Audit of sensitive actions

**[Requirement]** An append-only `audit` table, mirrored to `logs/audit.jsonl`, records:

- pairing and unpairing;
- unauthorized contacts;
- grants and revocations;
- plugin install, enable and disable;
- secret set and removal (names only);
- every approval decision (who, when, what, arguments hash);
- intent executions with external references;
- memory accept, edit and delete;
- budget overrides;
- policy changes.

**[Provisional]** Each row carries a hash chain (`prev_hash`, `hash`) for tamper evidence. `ka-agent audit verify` checks the chain.

## 9. MVP

### 9.1 In scope

**[Requirement]** The MVP is exactly:

- **One authorized user.** A single operator, paired by Telegram user id; the admin CLI is local.
- **One channel: Telegram.** Private chat, long polling, approvals through inline buttons, `/stop`, `/new`, `/status`.
- **One local workspace.** A single configured directory; all engines run there.
- **Resumable sessions through Ka.** Strands resume by path after any restart; policy is re-asserted on resume.
- **Editable stable memory.** Markdown files, `/remember`, `/memory` (show), `/forget`, `ka-agent memory edit`, compiled into `MEMORY.md`. Model-proposed candidates and their review arrive in Phase 2.
- **Searchable history.** FTS5 over prompt cycles, `/history`.
- **A minimal skills mechanism.** `SKILL.md` directories in `~/.config/ka-agent/skills/`, enabled with `ka-agent skills enable`, provisioned into the engine home, invoked with `/skill`.
- **Approvals for sensitive actions.** Ka asks become cards with a TTL and deny-on-expiry; the audit is complete.
- **A process supervisor with controlled restart.**
  - A systemd user unit for the daemon (`Restart=on-failure`, start-rate limiting).
  - The in-daemon supervisor for workers (backoff, crash-loop breaker).
  - `ka-agent restart` drains gracefully.

### 9.2 Out of scope (NOT in the MVP)

**[Requirement]** These are explicitly excluded from the MVP:

- Full multi-user (several operators or shared instances).
- Web UI.
- Third-party plugins. The MVP has no plugin runtime at all; first-party connectors arrive in Phase 3.
- Semantic search (embeddings, vector stores).
- Complex automations (multi-step workflows, triggers, webhooks). Even simple scheduled tasks wait for Phase 3.
- Multi-device sync.
- Additional channels (Discord, Slack, WhatsApp, email…).

### 9.3 MVP acceptance

**[Requirement]** The MVP is accepted only when every item below has been demonstrated:

- An end-to-end test drives a fake Telegram server against a worker that uses Ka's scripted speaker. It covers prompt, multi-chunk reply, approval allow/deny/expire, `/stop` mid-tool, worker kill, daemon kill and resume.
- 14 days of dogfooding with zero duplicated side effects and zero unaudited approvals. Every interrupted cycle was reported.
- A restore drill from backup succeeds.
- `ka` keeps working unchanged next to Ka Agent, with its strands, config and trust store untouched.

## 10. Roadmap

### Phase 0: research and Ka interface validation

**[Provisional]** Scope and order can change; the exit criteria are **[Requirement]**.

- **Objective:** turn §3.1 from code reading into executed evidence, and fix the KaPort contract.
- **Deliverables:**
  - A spike worker on pinned Ka crates.
  - A probe script per surface (`ka serve`, `ka acp`, `ka run`) that reproduces G1–G6, G19 and G20.
  - `docs/contract/ka-port.md` (operations, allowed command subset, event rules, replay rule, ask TTL).
  - The worker protocol spec.
  - Answers to the Phase 0 questions: `MEMORY.md` read timing, MCP naming and rule matching for `kagent.*`, and the Telegram limits are already answered in `docs/ka-interface-validation.md` (2026-10-15); cgroup kill semantics remain open.
- **Risks:** Ka crates are pre-1.0 and 0.3.0 is unreleased; the Ka API shape could change during the spike. The scripted speaker may not cover every event path.
- **Exit criteria:**
  - Every gap G1–G20 is marked confirmed, refuted or changed, with a reproduction.
  - The contract test suite passes against the pinned Ka version.
  - The boundary decision in §3.4 is ratified in an ADR.

### Phase 1: Telegram MVP

**[Provisional]** Phase scope can move; exit criteria are **[Requirement]**.

- **Objective:** deliver the whole §9.1 MVP: a usable resident assistant on Telegram with resumable sessions, approvals, editable stable memory, searchable history and minimal skills.
- **Deliverables:**
  - Daemon, admin CLI and worker.
  - SQLite schema v1 with migrations.
  - The Telegram adapter (pairing, allowlist, splitting, files, commands, approval cards).
  - Minimal memory (`/remember`, `/forget`, `ka-agent memory edit`, compiled `MEMORY.md`), the FTS history index with `/history`, and minimal skills (`ka-agent skills enable`, `/skill`).
  - The supervisor and systemd unit, budgets, the audit log, backups.
- **Risks:**
  - Telegram API edge cases (rate limits, edit failures).
  - Ka asks wedging a session if the TTL logic is wrong.
  - Approval cards that are too terse to judge.
- **Exit criteria:**
  - The §9.3 end-to-end fault tests pass in CI.
  - 14 days of dogfooding with no stuck session, no duplicated side effect and no unaudited action.
  - A restore drill passes, and `ka` still works unchanged next to Ka Agent.

### Phase 2: memory and history

**[Provisional]** Exit criteria are **[Requirement]**.

- **Objective:** make memory improve under the operator's control, and make history useful at scale.
- **Deliverables:**
  - Memory candidates with provenance: the `memory.propose` and `memory.search` tools through the bridge, and `/memory review`.
  - A budget for the compiled `MEMORY.md`, with retrieval for everything outside it.
  - Episode summaries (`ka run --mode plan`), retention policies, export and purge commands.
  - A labeled query set and recall evaluation for history search.
- **Risks:** memory pollution; the injected budget crowding the context; the cost of summary jobs.
- **Exit criteria:**
  - Every stable entry has provenance.
  - Accept and reject work end to end.
  - History search answers a 50-query labeled set with recall@10 ≥ 0.8, or else the embeddings decision (§4.3) is opened.
  - Export and purge pass round-trip tests.

### Phase 3: skills and approvals

**[Provisional]** Exit criteria are **[Requirement]**.

- **Objective:** extensible capabilities with safe external effects.
- **Deliverables:**
  - Skill installation from a git URL or path with a review step (diff plus approval), and skills bundled by plugins.
  - The MCP bridge with first-party connectors: filesystem extra roots, notes, RSS, HTTP, GitHub, calendar.
  - Scopes and grants; the intent log with idempotency.
  - The scheduler for reminders and read-only digests (`ka run --mode plan`); long-running task notifications.
- **Risks:**
  - Double prompting (Ka ask plus Ka Agent card).
  - Connector token scope creep.
  - The scheduler waking the machine or overspending.
- **Exit criteria:**
  - A fault-injection suite shows zero duplicate external writes across crashes in `executing`.
  - Every external_write is approved and audited.
  - The scheduled-job policy is fail-closed (no asks answered automatically).

### Phase 4: web UI

**[Provisional]** Exit criteria are **[Requirement]**.

- **Objective:** a local web UI for history, memory editing, approvals and settings that are safe to expose.
- **Deliverables:**
  - A loopback-only HTTP server in Ka Agent with session cookies created through a CLI-issued login link.
  - Views: sessions and transcripts (from Ka Agent's log and strands), the memory editor with diffs, the approvals inbox, budgets and audit.
- **Risks:** the web attack surface (CSRF, XSS); stack choice (§12); scope creep into a chat client.
- **Exit criteria:**
  - A security review checklist is passed.
  - It is unreachable from non-loopback addresses without an explicit reverse-proxy configuration.
  - It reaches feature parity for approvals with Telegram.

### Phase 5: plugin ecosystem

**[Provisional]** Exit criteria are **[Requirement]**.

- **Objective:** third-party plugins without trusting third-party code.
- **Deliverables:**
  - Plugin API v1 freeze.
  - MCP-stdio plugin runtime under bubblewrap with an egress proxy.
  - Signed archives and pinning.
  - `ka-agent plugin install|update|rm`.
  - Plugin authoring docs and an example plugin.
- **Risks:**
  - Supply-chain attacks through malicious or compromised plugins and skills, the main risk of any open marketplace.
  - Sandbox gaps on non-Linux hosts.
- **Exit criteria:**
  - Sandbox escape tests pass.
  - An unsigned or tampered plugin is refused.
  - A plugin can reach only its declared hosts and secrets.

### Phase 6: multi-channel and multi-user

**[Provisional]** Exit criteria are **[Requirement]**.

- **Objective:** more channels, and an explicit, chosen multi-user model.
- **Deliverables:**
  - A channel trait proven by a second adapter (e.g. Matrix or email).
  - Cross-channel identity linking for the same operator.
  - An ADR choosing between instance-per-user (default) and shared-instance multi-tenancy, with its isolation design.
- **Risks:** identity-linking mistakes that leak memory across people; channel feature mismatch (buttons, file sizes).
- **Exit criteria:**
  - Isolation tests prove that no memory, history or grant crosses identities.
  - The second channel passes the same end-to-end suite as Telegram.

## 11. Repository layout

### 11.1 Stack decision

**[Provisional]** Pure Rust (edition 2024) for daemon, CLI and worker, with tokio, rusqlite (bundled, FTS5) and serde. The reasons:

- The Ka crates are Rust.
- It gives one binary.
- It keeps the same strictness culture as Ka: deny `unwrap`, strict TOML.

The web UI stack is an open question (§12). **[Requirement]** Whatever it is, it lives behind the daemon's local HTTP API and never links Ka crates.

### 11.2 Layout

**[Provisional]** The brief's layout, adjusted. Crates are created only when their phase starts, with no empty placeholders; `ka-agent-web` therefore appears in Phase 4.

```
ka-agent/
  Cargo.toml                 # workspace; Ka crates pinned with "=" in [workspace.dependencies]
  crates/
    ka-agent-core/           # domain types and state machines (ids, sessions, approvals, intents); no I/O
    ka-agent-port/           # KaPort trait, worker protocol, crate-backed port; ONLY crate that may depend on ka-* crates
    ka-agent-store/          # SQLite schema, migrations, FTS5 index, backups, audit chain
    ka-agent-memory/         # stable memory files, candidates, MEMORY.md compiler, provenance
    ka-agent-runtime/        # session manager, durable queue, approvals, budgets, supervisor, MCP bridge, scheduler (Phase 3)
    ka-agent-channels/       # channel trait; telegram adapter (Bot API client, rendering, splitting)
    ka-agent-skills/         # SKILL.md registry, enablement, provisioning into the engine home
    ka-agent-plugins/        # Phase 3 (built-in connectors) → Phase 5 (sandboxed third-party runtime)
    ka-agent-web/            # Phase 4 only
  apps/
    ka-agent/                # the single binary: daemon, admin CLI, hidden `__worker` subcommand
  docs/
    adr/                     # one record per [Requirement] change
    contract/                # ka-port.md, worker-protocol.md, telegram.md, plugin-manifest.md
  examples/
    config/                  # ka-agent.toml, engine.toml (a Ka config layer)
    skills/                  # sample SKILL.md directories
  tests/
    e2e/                     # fake Telegram Bot API + scripted Ka speaker + fault injection
```

### 11.3 Dependency and CI rules

**[Requirement]** The workspace enforces these rules in CI:

- A CI gate, modeled on Ka's `cargo xtask deps` (`crates/xtask/src/main.rs:323-327`), fails if any crate other than `ka-agent-port` and `apps/ka-agent` depends on a `ka-*` crate.
- `ka-agent-core` has no I/O dependencies.
- Clippy denies `unwrap`/`expect`/`todo`; `unsafe_code` is forbidden. These mirror Ka's hard rules (`AGENTS.md:36`).
- Contract tests for the port run against the pinned Ka version, with the scripted speaker and no network. A Ka upgrade PR must keep them green.

## 12. Open questions

1. **Pure Rust vs Rust + TypeScript for the web.** Rust server-rendered pages (one binary, smaller attack surface) or a TypeScript SPA (richer UI, a second toolchain)? Decide at the Phase 4 start; the default leans to server-rendered Rust plus minimal JS. **Decided (M7, 2026-10-15): server-rendered Rust, no JavaScript at all** — a loopback-only HTTP/1.1 server in `ka-agent-web` (no framework, no asset pipeline, no CDN), session auth via a one-shot CLI-issued login link. See `docs/contract/web-ui.md`.
2. **`ka serve` vs a Rust library.** The MVP leans to the library in a worker (§3.4). This is revisited on every Ka release against the five serve criteria. Should Ka Agent also support driving a remote `ka serve` (Ka on another host)?
3. **Plugin model: process, WASM, sandbox or manifest convention.** MCP-stdio processes under bubblewrap reuse Ka's MCP client and any language. WASM (WASI components) gives stronger isolation but fewer SDKs and harder I/O. Is a manifest-only convention enough for first-party code? Decide before Phase 5. **Decided (M8, 2026-10-16): MCP-stdio processes under bubblewrap** — v1 without network egress (`--unshare-net`; `requires.network` must be empty, connectors stay first-party), ed25519-signed archives with TOFU pinning, and `exec`/`irreversible` effects not loadable for third parties. See `docs/contract/plugins.md`.
4. **Initial database.** Is SQLite (WAL, bundled FTS5) sufficient through Phase 6? When, if ever, does a server database become necessary (shared multi-user instance)? Should state and the index be split into two files to simplify backups?
5. **Embeddings strategy.** Local model through Ollama or LM Studio (privacy, cost) vs a provider API (quality)? Which vector store (sqlite-vec vs a separate index)? Which granularity (turn, summary, memory entry)? Gated by §4.3.
6. **Identity and multi-user model.** Instance-per-person vs a shared instance with per-user Ba? How are channel identities linked? Can an operator delegate approval rights (for example to a second device)? **Decided (M9, 2026-10-16): instance-per-user** (ADR-0001, `docs/adr/0001-instance-per-user.md`) — the data root is the isolation boundary; cross-channel linking is the `identities` table (`ka-agent identity`, CLI-only, audited), and delegation of approval rights stays open.
7. **Name and distribution.** The crates.io name `ka-agent` still holds Ka's legacy binary package (`research/ka/agent-boundary.md:64-88`). Should the product publish as `ka-agent` after Ka's Phase C, or under another package name with a `ka-agent` binary?
8. **Operator Ka config inheritance.** Should the worker optionally read `~/.config/ka/ka.toml` (provider settings, MCP servers) under Ka Agent's security overlay, or should the engine layer stay fully separate?

## 13. Immediate next steps

1. Validate the real Ka interface: pin the Ka crates at commit `a56aae2` (or 0.3.0 once published), build a throwaway worker with `spawn_with_speaker`, and run probes against `ka serve`, `ka acp` and `ka run` to confirm or refute G1–G20; record the results in `docs/ka-interface-validation.md`. The gap table and runtime probes now exist there (2026-10-15); the `spawn_with_speaker` worker now exists too (`crates/ka-agent-port`, scripted-speaker e2e in `crates/ka-agent-port/tests/`).
2. Design the minimal Ka ↔ Ka Agent contract: write `docs/contract/ka-port.md` and the `KaPort` trait (open, submit, answer, abort, close, events), the allowed command subset, the replay and `idle` rules, the ask TTL, and the worker NDJSON envelope.
3. Create the repository skeleton: Cargo workspace with `ka-agent-core`, `ka-agent-port`, `ka-agent-store` and `apps/ka-agent`, plus CI (fmt, clippy deny list, tests, and the gate that only the port may depend on Ka crates).
4. Define retry, restart and cancellation tests: encode the expected outcomes for worker kill, daemon kill mid-cycle and mid-approval, `/stop` mid-tool, provider retryable errors via the scripted speaker, Telegram 429/5xx, and duplicate `update_id`s as failing end-to-end tests.
5. Implement the worker and supervisor: the `__worker` subcommand, config composition with the security overlay, isolated `HOME`/`KA_DATA_DIR`/`XDG_STATE_HOME`, a cgroup per worker, crash detection, and backoff with a crash-loop breaker.
6. Implement session and memory persistence: SQLite schema v1 and migrations (sessions, bindings, turns, queue, outbox, approvals, audit, memory candidates), strand-id capture at bootstrap, resume by path with policy re-assertion, and the Markdown memory store with the `MEMORY.md` compiler.
7. Implement the Telegram adapter: long polling with persisted offsets, pairing and allowlist, message splitting and rendering, the file inbox, the command set, and inline-keyboard approval cards with operator checks on callbacks.
8. Implement approvals end to end: Ka `ask` → card → `answer`, TTL deny, abort while awaiting approval, audit rows written before answers, and budget asks from `[guards]`.
9. Implement history, memory and skills commands: the FTS5 index fed per prompt cycle, `/history`, `/remember`, `/memory`, `/forget`, `ka-agent memory edit`, and minimal skills enablement with `/skill`.
10. Package and dogfood: the systemd user unit, `ka-agent doctor` (including Ka version and contract hash), nightly backups with a restore drill, then 14 days of dogfooding and the Phase 1 exit review against §9.3. **Shipped (M10, 2026-10-16): units in `dist/systemd/` (service + nightly backup timer, ADR-0001's instance-per-user shape), `doctor` with the pin/contract-hash/sandbox/plugins/identities state, `ka-agent backup|restore` with the drill in `ops_e2e.rs`. The 14-day window is the remaining item: start it from the installed unit per `docs/ops/dogfooding.md`, then run the §9.3 exit review.
