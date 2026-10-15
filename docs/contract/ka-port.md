# Ka ↔ Ka Agent contract: `KaPort`

Status: M1 contract, 2026-10-15. This document defines the *only* surface
through which Ka Agent talks to a Ka engine. Everything above `ka-agent-port`
is written against the `KaPort` trait; swapping the boundary implementation
(e.g. to `ka serve` once it qualifies, SEED §3.4) must stay a one-crate change.

Evidence paths (`crates/…`) point into the Ka repository at commit
`a56aae2d1b3a4c31c4910979e1033002eacc53a0`. Runtime probes (V-xx) are in
`docs/ka-interface-validation.md`.

The wire format between the daemon and the worker process is specified in
[`worker-protocol.md`](worker-protocol.md); this document specifies the trait
and the behavioral rules.

## 1. The trait

```rust
pub trait KaPort: Send {
    fn open_session(&mut self, req: OpenRequest) -> Result<SessionHandle, PortError>;
    fn submit(&mut self, prompt: Prompt) -> Result<TurnSpan, PortError>;
    fn answer(&mut self, ask_id: AskId, choice: AnswerChoice) -> Result<(), PortError>;
    fn abort(&mut self) -> Result<(), PortError>;
    fn set_mode(&mut self, mode: Mode) -> Result<(), PortError>;
    fn set_model(&mut self, selector: String) -> Result<(), PortError>;
    fn set_name(&mut self, name: String) -> Result<(), PortError>;
    fn compact(&mut self) -> Result<(), PortError>;
    fn close(&mut self) -> Result<(), PortError>;
    fn events(&mut self) -> &mut dyn Stream<Item = PortEvent>;
}
```

| Operation | Ka command | Meaning |
|---|---|---|
| `open_session` | engine spawn + attach (`StrandChoice::{New,Path}`) | Start or resume one engine on one strand, with explicit model/mode and Ka Agent's config layers. Returns a handle carrying the strand id (from `session_info`). |
| `submit` | `prompt` | Begin one prompt cycle. Returns the span descriptor; live output is the event stream until `idle`. |
| `answer` | `answer` | Resolve a pending Ka `ask` (`allow` / `always` / `deny`). |
| `abort` | `abort` | Cancel the running prompt cycle; engine emits `turn_finished{stop:"aborted"}` then `idle`. |
| `set_mode` | `set_mode` | Set permission mode; persists a `Change` record to the strand. |
| `set_model` | `set_model` | Set model selector; persists a `Change` record to the strand. |
| `set_name` | `set_name` | Name the session (title). |
| `compact` | `compact` | Compact the strand into a digest (offered on `overflow`). |
| `close` | session drop | Close the engine and release the strand. |

## 2. Allowed command subset and the trust invariant

**[Requirement]** Ka Agent forwards only these Ka `Command`s: `prompt`,
`answer`, `abort`, `set_mode`, `set_model`, `set_name`, `compact`.

Everything else in Ka's `Command` enum is engine-internal or local-only, and
**is NEVER forwarded**:

- `shell` — executes a raw shell on the host, outside the permission engine
  (`crates/ka-protocol/src/lib.rs:316-327`);
- `set_feature`, `set_sandbox` — local-only surface controls
  (`crates/ka-protocol/src/lib.rs:355-379`);
- `save_api_key`, `save_settings` — write configuration files
  (`crates/ka-protocol/src/lib.rs:428-464`).

**Trust invariant:** no input arriving from a channel (Telegram today) can
ever cause one of these commands to be sent. The port API above simply has no
operation that maps to them; the invariant is enforced by construction and by
contract tests. `always` answers are also never sent from remote channels (Ka's
"always" is a coarse per-tool-name allow written into the project `.ka/ka.toml`,
gap G12); durable grants live in Ka Agent's own rules layer instead.

## 3. Event mapping (ka `Event` → `PortEvent`)

| Ka event | PortEvent | Notes |
|---|---|---|
| `session_info` | `Bootstrap::Info{strand_id, …}` | Strand id capture; arrives first (probe V-03). |
| `mode_changed`, `effort_changed`, `model_changed` | `Bootstrap::State{…}` | Bootstrap state echo. |
| `replay` | `Bootstrap::Replay{…}` | Exactly one per attach (`crates/ka-protocol/src/lib.rs:561-566`). Never forwarded to a channel; used only for continuity checking (SEED §3.7). |
| `inventory`, `todos`, `title`, `presence` | `Bootstrap::Meta{…}` | Post-replay metadata. |
| `turn_started` | `TurnStarted{turn}` | Start of a turn inside the live cycle. |
| `delta` (text/thought/call) | `Delta{kind, text}` | Streaming output. |
| `call_started` / `call_output` / `call_finished` | `ToolCall{…}` phases | Tool activity rendering. |
| `context_meter` | `ContextMeter{…}` | Budget accounting. |
| `turn_finished` | `TurnFinished{stop, usage}` | Includes usage/cost; `stop ∈ {done, aborted, error, …}`. |
| `ask` | `Ask{ask_id, subject, choices}` | Permission prompt; see §5. |
| `error` | `Error{class, retryable, message}` | See §7. |
| `idle` | `Idle` | **Prompt-cycle boundary**; see §4. |
| unknown / future event types | `Unknown{raw}` | Lenient decode (gap G7: closed enums, no version field). Never fatal. |

**Bootstrap vs live:** on attach, everything up to and including `replay` (plus
the post-replay metadata burst) is *bootstrap*: replayed history, not new
output. **Everything after Ka Agent's own `submit`, up to and including the
next `idle`, is live output** and is the only thing ever delivered to a channel
as new (SEED §3.7 [Requirement]).

## 4. Single-writer rule (one prompt in flight)

**[Requirement]** Exactly one `submit` per session is in flight. Ka silently
drops a `prompt` that arrives while a cycle is running (gap G4; probes V-06,
V-06b — HTTP 200, no events for the dropped prompt). Ka Agent therefore:

- keeps a durable queue per session; inbound messages are enqueued, not sent;
- drains the next queued item only after `idle` is observed;
- treats the span `submit → … → idle` as one Ka Agent turn (`turn_id`), which
  is valid *because* of the single-writer rule (a cycle may contain several
  `turn_started…turn_finished` pairs: auto-poke, verify, queued input —
  `idle` is the delivery boundary, `crates/ka-protocol/src/lib.rs:681-683`).
- does **not** end the turn on an `idle` that arrives while the turn is still
  open (`submitted`/`running` in the store) or while an ask is pending. Ka
  reports `idle` for the cycle during worker start-up and around its
  permission gate, and ending the turn there makes the next message overtake
  the running one (nothing queues behind it) and orphans the ask. The turn's
  own terminal status ends the turn; the ask's decision or TTL resolves the
  ask;
- returns the session to `idle` at `turn_finished` — the turn boundary — when
  no ask is pending and the queue is empty, and only then. `idle` remains the
  delivery boundary (flush, episodic index, queue drain), and it never drains
  the queue into a live cycle (`PortError::Busy` is a dropped prompt).

`submit` while a cycle is live is a caller bug: the port returns
`PortError::Busy` and never sends the command.

## 5. Asks: TTL and deny-on-expiry

Ka asks block the engine until `answer` or `abort`, with **no timeout** (gap
G3; `crates/ka-engine/src/voice.rs:2786-2823`). Ka Agent therefore owns expiry:

- each `ask` becomes an approval card with `approval_ttl_secs` (daemon config,
  default 900 s);
- if the TTL lapses with no operator decision, Ka Agent answers **`deny`**
  (fail closed) and records the expiry in the audit log;
- `abort` while awaiting an ask is allowed and cancels the cycle;
- an `ask` that arrives when no turn is open (the engine reported `idle`
  before its gate, or the turn already finished) is answered **`deny`**
  immediately and recorded as a settled approval: no surface could act on it,
  and leaving it pending would block the engine on its permission gate
  forever.

## 6. Resume and mandatory re-assertion

**[Requirement]** Sessions resume lazily: the first inbound message after a
(daemon/worker/machine) restart spawns a worker with
`StrandChoice::Path(strand_path)`. A session whose recorded strand file does
not exist yet (Ka appends it only after the first completed turn) is reopened
with `StrandChoice::New` (SEED §3.5).

**[Requirement — validation finding F-1 / probe V-10]:** a resumed strand
restores model/mode/effort **only from `Change` records**, which Ka appends
only for mid-session `set_model`/`set_mode`/`set_effort` and overflow
promotion — never for the initial config
(`crates/ka-engine/src/engine.rs:1639`, `:1661`, `:1676`, `:2930`). A resumed
strand can therefore come back **model-less** and in whatever mode the strand
last recorded. After every attach, Ka Agent **must** send `set_mode` and
`set_model` before the first `submit`. This both re-asserts policy (a strand
recorded in a looser mode cannot loosen it) and persists fresh `Change`
records, making the next resume self-restoring.

## 7. Error classes and runtime reactions

Ka retries internally (wire level, then 5 s / 20 s / 60 s, then fallback
models). When retries are exhausted, Ka Agent receives `error{class}` followed
by `turn_finished{stop:"error"}` and reacts by class (SEED §3.8
[Requirement]):

| `ErrorClass` | Ka Agent reaction |
|---|---|
| `auth` | Pause the instance; alert the operator via Telegram and the CLI. |
| `rate_limit` | Report it; offer a **Retry** button (no auto-retry). |
| `network` | Report it; offer a **Retry** button (no auto-retry). |
| `overflow` | Offer `/compact` (Ka's `compact`) or `/new`. |
| `protocol`, `unsupported`, `internal` | Report a short summary; log details. |

Nothing that may have had side effects is ever retried automatically. Worker
crash (event channel closed without `idle`, gap G13): mark the cycle
`interrupted`, notify the user, respawn on the next message with backoff
(1 s → 60 s), crash-loop breaker at `crash_max` crashes in `crash_window_secs`
(see `[worker_restart]` in `ka-agent.toml`).

## 8. Kill semantics and `EngineClosed` detection

- **Graceful stop:** `abort`, then wait `turn_grace_secs` (default 10 s,
  provisional) for `turn_finished{stop:"aborted"}` + `idle`.
- **Escalation:** if grace lapses, SIGTERM to the worker's **process group**
  (so tool children die too; the MVP engine layer sets
  `[tools.bash] background_after_ms = 0` so nothing detaches, gap G15), then
  SIGKILL after a further 5 s. The turn is marked `aborted` or `killed`.
- **EngineClosed detection:** the worker reports `EngineClosed{reason}` (see
  `worker-protocol.md`) and exits with code 3 when the engine channel closes
  without a terminal `idle`. The daemon treats any worker exit ≠ 0 without
  `idle` as a crash: mark `interrupted`, notify, backoff-respawn on the next
  message (never auto-replay the lost prompt; SEED §3.6).
