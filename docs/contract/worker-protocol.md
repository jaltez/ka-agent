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
| `Open` — start/attach an engine on a strand | `{"op":"open","workspace":"/home/op/work","strand":"/home/op/.local/share/ka-agent/ka/strands/01J….jsonl","model":"anthropic/claude-sonnet-4-5","mode":"guarded","engine_layer_toml":"/home/op/.config/ka-agent/engine.toml","scenario":null}` |
| `Submit` — one prompt (text, optional images, optional skills) | `{"op":"submit","text":"fix the failing test","images":[],"skills":[]}` |
| `Answer` — resolve a pending ask | `{"op":"answer","ask_id":"a-17","choice":"allow"}` |
| `Abort` — cancel the running cycle | `{"op":"abort"}` |
| `SetModel` — set model selector | `{"op":"set_model","selector":"openai/gpt-5.2"}` |
| `SetMode` — set permission mode | `{"op":"set_mode","mode":"plan"}` |
| `SetName` — title the session | `{"op":"set_name","name":"CI flake hunt"}` |
| `Compact` — compact the strand | `{"op":"compact"}` |
| `Close` — close the engine and exit cleanly | `{"op":"close"}` |

Field notes:

- `Open.strand`: the **sentinel `"new"`** (fresh strand) or a strand
  **path** string (resume). `null` is not valid — the field deserializes
  exactly those two shapes (`proto.rs: StrandSel`).
- `Open.mode`: `"guarded" | "accept_edits" | "free" | "plan"`.
- `Open.engine_layer_toml`: the ka engine layer **document itself** (ka's
  strict `ka.toml` grammar, as text — the daemon reads
  `examples/config/engine.toml` off disk and passes its contents). The
  worker's security overlay is applied **after** it and always wins.
- `Open.scenario`: reserved for the scripted-speaker test scenario (contract
  tests, `spawn_with_speaker`); `null` in production.
- `Submit.images`: image inputs for multimodal prompts; `Submit.skills`:
  skill names to enable for this turn.
- `Answer.choice`: a **numeric index into the ask's `options`** in request
  order (`0` = allow, `1` = always, `2` = deny for the standard engine
  ordering). Remote channels never send the always index (gap G12).
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
