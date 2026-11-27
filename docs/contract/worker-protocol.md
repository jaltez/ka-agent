# Worker protocol — NDJSON over stdio

Status: M1 contract, 2026-10-15. The daemon spawns one worker process
(`ka-agent __worker`) per active session. The worker links the pinned Ka
crates, runs the engine in-process, and speaks this line-delimited JSON
protocol with the daemon over stdin/stdout. One **JSON object per line**
(NDJSON); UTF-8; no trailing data; stderr is captured as worker diagnostics,
never parsed as protocol.

Field names below are normative and must match `ka-agent-port` exactly.
The behavioral contract (single-writer, TTL, kill semantics) is in
[`ka-port.md`](ka-port.md).

## 1. Messages

Direction: D→W (daemon to worker, written to the worker's stdin), W→D
(worker to daemon, written to stdout).

### D→W ops (`"op"`)

| op | line |
|---|---|
| `Open` — start/attach an engine on a strand | `{"op":"open","workspace":"/home/op/work","strand":"/home/op/.ka-agent/profiles/default/ka/strands/01J….jsonl","model":"anthropic/claude-sonnet-4-5","mode":"ask","engine_layer_toml":"/home/op/.config/ka-agent/engine.toml","scenario":null,"auto_allow":[]}` |
| `Submit` — one prompt (text, optional images, optional skills, optional tool allowlist) | `{"op":"submit","text":"fix the failing test","images":[],"skills":[],"allowed_tools":["read"]}` |
| `Answer` — resolve a pending ask | `{"op":"answer","ask_id":"ask-17","choice":2}` |
| `Abort` — cancel the running cycle | `{"op":"abort"}` |
| `SetModel` — set model selector | `{"op":"set_model","selector":"openai/gpt-5.2"}` |
| `SetMode` — set permission mode | `{"op":"set_mode","mode":"readonly"}` |
| `SetName` — title the session | `{"op":"set_name","name":"CI flake hunt"}` |
| `SetEffort` — set effort level | `{"op":"set_effort","level":"high"}` |
| `Steer` — queue text into the live cycle (the engine drains it at turn/tool boundaries; settles into the queue when unconsumed) | `{"op":"steer","text":"also update the README"}` |
| `UndoFile` — revert the engine's last file edit | `{"op":"undo_file"}` |
| `Compact` — compact the strand | `{"op":"compact"}` |
| `SessionDiff` — the session's cumulative file diff | `{"op":"session_diff"}` |
| `ContextBreakdown` — the context-usage breakdown | `{"op":"context_breakdown"}` |
| `SideQuestion` — ask the engine's fast role a side question | `{"op":"side_question","text":"why does the spill GC keep the newest file?"}` |
| `Close` — close the engine and exit cleanly | `{"op":"close"}` |

Field notes:

- `Open.strand`: the **sentinel `"new"`** (fresh strand) or a strand
  **path** string (resume). `null` is not valid — the field deserializes
  exactly those two shapes (`proto.rs: StrandSel`).
- `Open.mode`: `"ask" | "accept_edits" | "allow" | "readonly"` (the old
  `guarded`/`free`/`plan` spellings are accepted as aliases). The worker
  translates to the pinned engine's own words on its wire.
- `Open.engine_layer_toml`: the ka engine layer **document itself** (ka's
  strict `ka.toml` grammar, as text — the daemon reads
  `examples/config/engine.toml` off disk and passes its contents). The
  worker's security overlay is applied **after** it and always wins.
- `Open.scenario`: reserved for the scripted-speaker test scenario (contract
  tests, `spawn_with_speaker`); `null` in production.
- `Open.auto_allow`: exact tool names that never ask (the daemon's approval
  profile: reads, proposals and local writes); the worker inserts them as
  allow rules ahead of every other rule. Absent = none.
- `SessionDiff`, `ContextBreakdown`, `SideQuestion` (v0.12.2): the three
  session queries. None starts a turn; each settles with its answer and
  then `idle` — `context_breakdown` as the ka `context_breakdown` event,
  the other two as ka `note` lines the port types by their marker
  (`ka-port.md` §3). `side_question` is one provider call on the fast
  role (`[roles] fast`).
- Security overlay (always applied after the layer, always wins): `mode`
  from `Open.mode` (else `ask`); `[tools.bash] background_after_ms = 0`;
  `[features] disable` gains `tool:remember`, `tool:cron`, `tool:monitor`,
  `tool:team`, `tool:checkpoint` and `tool:generate_image` (an entry the
  layer already carries is not repeated); the `__guard` hooks; the
  `kagent` MCP bridge (`mcp-bridge.md`). A `[permissions] profile`
  activation in the layer refuses the open at composition time
  (`ka-agent-port/src/engine_layer.rs`).

## 1.1 Worker lifetime and the idle reap

The daemon keeps at most one worker per session and stops it when
`worker_idle_timeout_secs` (default 900; `0` = keep forever) of session
inactivity pass **and** the session is fully settled — idle status, no
open turn, no pending ask, nothing queued. The stop is a `Close`-grade
kill flagged `deliberate_close`: it books **no crash**, no backoff, no
degradation. The next message starts a fresh worker that resumes the
strand by path (the §3.6 resume path, replay noted, nothing re-delivered).
Verified by `idle_worker_is_stopped_then_restarted_on_demand` (chat e2e).
- `Submit.images`: image inputs for multimodal prompts; `Submit.skills`:
  skill names to enable for this turn; `Submit.allowed_tools` (optional):
  exact hand names this turn may use (`null` = every tool the engine
  exposes). Absent = `null`, so an older daemon stays compatible.
- `Answer.choice`: a **numeric index into the ask's `options`** in request
  order. A permission ask (`allow`, `always`, `deny`) takes `0` = allow,
  `1` = always, `2` = deny — remote channels never send the always index
  (gap G12). Any other ask on the same event (the engine's
  `continue`/`stop` session guards, a checkpoint confirmation, an MCP
  elicitation) is answered by its own options: `0` takes the first, the
  last index is the decline, and the daemon never sends `always` for it
  (`ka-port.md` §5).
- After `open`, the daemon **must** send `set_mode` and `set_model` before
  the first `submit` (validation finding F-1; see `ka-port.md` §6).

### W→D events (`"ev"`)

| ev | line |
|---|---|
| `Ready` — worker up, engine spawn pending `open` | `{"ev":"ready"}` |

Spawning: the daemon runs `binary` with `OpenRequest.worker_args` as argv
prefix (the app passes `["__worker"]`; the port test worker passes none),
then `--scenario <path>` when a scripted speaker is requested, then the
NDJSON protocol below over stdio.
| `Strand` — emitted after `open`, once the strand id is known (from `session_info`) | `{"ev":"strand","id":"01J8Z0W7…"}` |
| `Ka` — one wrapped ka `Event` (verbatim serde form of ka-protocol's `Event`) | `{"ev":"ka","event":{"type":"delta","kind":"text","text":"Hello"}}` |
| `EngineClosed` — engine event channel closed (with reason) | `{"ev":"engine_closed","reason":"engine task exited without idle"}` |
| `Fatal` — unrecoverable worker error | `{"ev":"fatal","message":"open strand: record decode error"}` |

Event ordering: `Ready` first; after `open` comes `Strand`, then the ka
bootstrap events (`session_info`, `mode_changed`, `effort_changed`, `replay`,
`inventory`, `todos` — probe V-03) wrapped in `Ka`. Every ka event — bootstrap
and live — is forwarded as a `Ka` line; classifying bootstrap vs live
(`idle` boundary) is the daemon's job via the port.

## 2. Environment isolation

**[Requirement]** The daemon sets the worker's environment; the worker never
inherits the operator's interactive Ka state (validation §7: a worker that
inherits ambient config silently uses the operator's model, mode **and keys**).
Strands, spills, trust store and jobs live under the worker's own
`KA_DATA_DIR` (validation §5); `XDG_CONFIG_HOME` must be pinned because ka's
user config layer resolves through it, bypassing `HOME` (finding F-2).

| Variable | Value |
|---|---|
| `HOME` | `<instance>/engine-home` (compiled `MEMORY.md`, skills) |
| `XDG_CONFIG_HOME` | `<instance>/engine-home/.config` (pinned; never inherited) |
| `XDG_STATE_HOME` | `<instance>/engine-state` (spills, trust, jobs) |
| `XDG_CACHE_HOME` | `<instance>/engine-home/.cache` (pinned; feeds the sandbox allowlist) |
| `KA_DATA_DIR` | `<instance>/ka` (strands, waypoints) |

Additionally, any ambient variable whose name contains `KEY`, `TOKEN`,
`SECRET`, `PASSWORD` or `AUTH` is **stripped**; provider credentials are
injected per session from Ka Agent's secret store, only when the session's
model needs them.

## 3. Exit codes

| Code | Meaning |
|---|---|
| 0 | Clean exit: `close` received, or startup refused with a `Fatal` that is a caller error. |
| 3 | Engine closed unexpectedly: event channel closed without a terminal `idle` (crash, gap G13). Preceded by an `EngineClosed` line when possible. |

Any other exit code (or death by signal) is treated by the daemon as a worker
crash: mark the in-flight turn `interrupted`, notify, respawn with backoff on
the next message.
