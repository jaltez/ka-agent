# Ka ↔ Ka Agent contract: `KaPort`

Status: M1 contract, 2026-10-15. This document defines the *only* surface
through which Ka Agent talks to a Ka engine. Everything above `ka-agent-port`
is written against the `KaPort` trait; swapping the boundary implementation
must stay a one-crate change (SEED §3.4).

Evidence paths (`crates/…`) point into the Ka repository at commit
`a56aae2d1b3a4c31c4910979e1033002eacc53a0`; paths marked *v0.12.2* point at
the pinned revision `f41ecf9`. Runtime probes (V-xx) are in
`docs/ka-interface-validation.md`.

The wire format between the daemon and the worker process is specified in
[`worker-protocol.md`](worker-protocol.md); this document specifies the trait
and the behavioral rules.

## 1. The trait

The trait is deliberately minimal: **one method**. Everything else a session
does lives on the returned handle, not the trait, so the port stays swappable
with a single construction site.

```rust
pub trait KaPort: Send + Sync {
    /// Start or resume one engine on one strand. Returns the live session
    /// handle with its event stream.
    fn open<'a>(&'a self, req: OpenRequest)
        -> BoxFut<'a, Result<PortSession, PortError>>;
}
```

The handle (`PortSession`) exposes the rest; the table maps handle methods
to what they send over the worker protocol (`worker-protocol.md`):

| Operation | Ka command | Meaning |
|---|---|---|
| `open` | engine spawn + attach (`StrandSel::{New,Path}`) | Start or resume one engine on one strand, with explicit model/mode and Ka Agent's config layers. Returns a handle carrying the strand id (from `session_info`). |
| `submit` | `prompt` | Begin one prompt cycle. Returns the span descriptor; live output is the event stream until `idle`. |
| `answer` | `answer` | Resolve a pending Ka `ask` by option index, decided by the ask's shape: a permission ask keeps `allow` / `always` / `deny` (0 / 1 / 2); any other ask takes `0` for allow and its last option — the decline — for deny (§5). |
| `abort` | `abort` | Cancel the running prompt cycle; engine emits `turn_finished{stop:"aborted"}` then `idle`. |
| `set_mode` | `set_mode` | Set permission mode; persists a `Change` record to the strand. |
| `set_model` | `set_model` | Set model selector; persists a `Change` record to the strand. |
| `set_name` | `set_name` | Name the session (title). |
| `set_effort` | `set_effort` | Set effort level; persists a `Change` record to the strand. |
| `steer` | `steer` | Queue text into the **live** cycle — the engine drains it at turn/tool boundaries and settles unconsumed steers into the session queue (`busy_messages = "steer"` default; not a second `submit`, so the single-writer rule holds). |
| `undo_file` | `undo_file` | Ask the engine to revert its last file edit. |
| `compact` | `compact` | Compact the strand into a digest (offered on `overflow`). |
| `session_diff` | `session_diff` | The session's cumulative file diff (`/diff`): every file the session mutated, its pre-mutation original against the disk. Read-only. The answer is `SessionDiff`, then `idle` (§3). |
| `context_breakdown` | `context_breakdown` | The context-usage breakdown (`/context`). The answer is `ContextBreakdown`, then `idle` (§3). |
| `side_question` | `side_question` | Ask the engine's fast role (`[roles] fast`) a side question (`/btw`): tools off, the transcript and the context untouched, one provider call on the fast model. The answer is `SideAnswer`, then `idle` (§3). |
| `close` | session drop | Close the engine and release the strand. |

## 2. Allowed command subset and the trust invariant

**[Requirement]** Ka Agent forwards only these Ka `Command`s: `prompt`,
`answer`, `abort`, `set_mode`, `set_model`, `set_name`, `set_effort`,
`steer`, `undo_file`, `compact`, `session_diff`, `context_breakdown`,
`side_question` (the last three since the v0.12.2 pin).

Everything else in Ka's `Command` enum is engine-internal or local-only, and
**is NEVER forwarded**:

- `shell` — executes a raw shell on the host, outside the permission engine
  (`crates/ka-protocol/src/lib.rs:316-327`);
- `set_feature`, `set_sandbox` — local-only surface controls
  (`crates/ka-protocol/src/lib.rs:355-379`);
- `save_api_key`, `save_settings` — write configuration files
  (`crates/ka-protocol/src/lib.rs:428-464`);
- the session-scoped automation — `goal`, `loop`, `checkpoint` /
  `restore_checkpoint`, `tan`, `switch_strand`, `fork_strand` — which
  would run inside the engine with no principal, budget or run row, and
  outside the daemon's scheduler and cancellation. The worker's overlay
  disables the matching hands (`cron`, `monitor`, `team`, `checkpoint`;
  `worker-protocol.md` §1).

**Trust invariant:** no input arriving from a channel (Telegram today) can
ever cause one of these commands to be sent. The port API above simply has no
operation that maps to them; the invariant is enforced by construction and by
contract tests. `always` answers are also never sent from remote channels (Ka's
"always" is a coarse per-tool-name allow written into the project `.ka/ka.toml`,
gap G12); durable grants live in Ka Agent's own rules layer instead.

## 3. Event mapping (ka `Event` → `PortEvent`)

| Ka event | PortEvent | Notes |
|---|---|---|
| `session_info` | — (captured) | Strand identity: the worker resolves the id to its file and reports it as the `strand` line (`worker-protocol.md` §1); it fills `strand_id` / `strand_path` on the next `Bootstrap`. Arrives first (probe V-03). |
| `replay` | `Bootstrap{strand_id, strand_path, replay_messages}` | Exactly one per attach. Never forwarded to a channel; used only for continuity checking (SEED §3.7). |
| `mode_changed`, `effort_changed`, `features_changed` | — (dropped) | Bootstrap state echo, verified by contract tests rather than streamed. |
| `model_changed` | `ModelChanged(selector)` | A fallback hop or a live selector change: surfaces show the model actually answering (F3). |
| `inventory` | `Inventory{tools}` | The hands this engine offers (after the layer, the role's allowlist and the worker's overlay). |
| `todos` | `Plan([(text, state)])` | Whole-list replacement, display order. |
| `title` | `Title(title)` | |
| `digest_started` / `digest_finished` | `Compacting{done}` | |
| `turn_started` | `TurnStarted` | Start of a turn inside the live cycle. |
| `delta` (text / thought) | `DeltaText(text)` / `DeltaThought(text)` | Streaming output. |
| `delta` (call) | — (dropped) | `CallStarted` carries the same call. |
| `call_started` / `call_finished` / `call_output` | `CallStarted{tool, id, detail}` / `CallFinished{tool, id, ok}` / `CallOutput{tool, id, excerpt, is_error}` | Tool activity rendering. |
| `context_meter` | `Context{used, window}` | Budget accounting. |
| `context_breakdown` | `ContextBreakdown{parts, window}` | The answer to `context_breakdown` (v0.12.2): `(name, tokens)` per component in engine order, `window` of the model in use (0 = unknown). |
| `turn_finished` | `TurnFinished{stop, cost_usd, tokens}` | `tokens` sums input + output + cache read + cache write; `stop ∈ {done, length, aborted, error}`. |
| `ask` | `Ask{ask_id, question, detail, options}` | The first question, verbatim: a permission prompt, or any other question the engine poses on the same event; see §5. |
| `note` | `Note(text)` | Informational. Three notes carry an answer instead (below). |
| `note` starting `btw: ` | `SideAnswer{text}` | The answer to `side_question`, the fast role's text with the marker removed. |
| `note` starting `session diff: ` | `SessionDiff{summary}` | The answer to `session_diff`: `no changes`, or `N file(s), X +/- lines — full: spill://<id>`, a blank line, an 8k-capped excerpt. The full diff is the spill file: `ka_agent_port::spill_path(state_dir, pointer)` resolves the pointer under the worker's `XDG_STATE_HOME` (`<state>/ka/spills/<id>`, where ka parks spills). |
| `note` starting `idle recap: ` | `Recap{text}` | An idle recap (`recap.idle_minutes`): what the fast role made of the session after the quiet interval; also recorded on the strand as `x.ka.recap`. |
| `error` | `EngineError{class, retryable, message}` | See §7. A worker `Fatal` arrives as `EngineError{class: "fatal"}`. |
| `idle` | `Idle` | **Prompt-cycle boundary**; see §4. |
| `presence`, `shell_output`, `tasks`, `task_detail`, `debug_roster` | — (dropped) | Surface-specific. |
| an undecodable line | — (dropped) | Lenient decode (gap G7: closed enums, no version field): the reader skips a line it cannot decode and keeps reading (`crates/ka-agent-port/src/port/session.rs`, `reader_task`). Never fatal, and never surfaced — there is no `Unknown` variant. |

**Answers carried on notes (v0.12.2).** The engine has no event type for
the `side_question` and `session_diff` answers or for an idle recap: each
rides `note` with a marker prefix. The prefix is read in exactly one place
(`crates/ka-agent-port/src/port/events.rs`, `project_note`); the engine's
degrade notes (`side question unavailable`, `idle recap unavailable: …`)
carry no answer and stay `Note`s. The markers are engine-authored, but a
note is plain text: a `.ka/hooks` failure reason reaches `note` unprefixed,
so a hook that prints one of these prefixes projects as that answer.

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
  The runtime announces that turn to its surfaces exactly once: the first
  `turn_started` of the cycle becomes the single `AgentEvent::TurnStarted`
  (the `turn_id` is claimed before `submit`, so it is already known), and
  later engine starts in the same cycle are not re-announced;
- does **not** end the turn on an `idle` that arrives while the turn is still
  open (`submitted`/`running` in the store) or while an ask is pending. Ka
  reports `idle` for the cycle during worker start-up and around its
  permission gate, and ending the turn there makes the next message overtake
  the running one (nothing queues behind it) and orphans the ask. The turn's
  own terminal status ends the turn; the ask's decision or TTL resolves the
  ask;
- returns the session to `idle` at `turn_finished` — the turn boundary — when
  no ask is pending and the queue is empty, and only then. The turn record
  going terminal is what proves the cycle is over, so the status never
  depends on catching a transient `idle`: that event remains the delivery
  boundary (flush, episodic index, queue drain), and it never drains the
  queue into a live cycle (a mid-cycle `prompt` is dropped by the engine —
  hence the runtime owns the queue and `drain_queue` is the only submitter;
  steering uses `steer`, not `submit`).

`submit` while a cycle is live is a caller bug: the port layer never sends
it — `drain_queue` is the only submitter, so the situation cannot arise by
construction. (The refusal is runtime-owned; there is no `PortError::Busy`
variant — the enum is `Spawn/Io/Protocol/Closed/EngineClosed`,
`crates/ka-agent-port/src/port/mod.rs:30-50`.)

## 5. Asks: shape, TTL and deny-on-expiry

Ka asks block the engine until `answer` or `abort`, with **no timeout** (gap
G3; `crates/ka-engine/src/voice.rs:2786-2823`). Ka Agent therefore owns expiry:

**[Requirement — ask shape]** Ka Agent answers an ask by its shape
(`crates/ka-agent-core/src/approval.rs`, `is_permission_ask` and
`ka_choice_for`). A **permission ask** is an ask whose options are exactly
`allow`, `always`, `deny` — the only kind a grant may answer (`always`,
allow-for-session and the durable rules of `grants.md`). Every other ask
arrives on the same `ask` event with its own options and ends with its
decline option: the engine's `continue`/`stop` session guards, a
checkpoint-restore confirmation, an MCP elicitation. For those, `allow`
answers index 0 (the first option), `deny` the last index (the decline),
`always` is refused, and the card shows the ask's own options. An ask that
offers no option is never answered by index — the cycle is aborted instead,
because the engine takes an index as it comes.

- each `ask` becomes an approval card with `approval_ttl_secs` (daemon config,
  default 900 s);
- if the TTL lapses with no operator decision, Ka Agent answers the ask's
  **decline** (`deny` on a permission ask; fail closed) and records the
  expiry in the audit log;
- `abort` while awaiting an ask is allowed and cancels the cycle;
- an `ask` that arrives when no turn is open (the engine reported `idle`
  before its gate, or the turn already finished) is answered its
  **decline** immediately and recorded as a settled approval: no surface
  could act on it, and leaving it pending would block the engine on its
  ask forever.

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
| `rate_limit` | Report it; offer a **Retry** affordance (no auto-retry): `/retry` on every surface, the ↻ inline button on Telegram for any error whose `retryable` flag is set, and `ka-agent ask --wait` for scripts. `Runtime::retry_last` re-submits the stored `inbound_text` of the newest `error`/`interrupted` turn only — never automatically. |
| `network` | Report it; offer the same **Retry** affordance (no auto-retry). |
| `overflow` | Offer `/compact` (Ka's `compact`) or `/new`. |
| `protocol`, `unsupported`, `internal` | Report a short summary; log details. |

The `error` envelope carries Ka's own `retryable` flag, and the phone's
↻ button is driven by that flag (not a hard-coded class list); the
engine remains the sole judge of what a retry can fix.

Nothing that may have had side effects is ever retried automatically. Worker
crash (event channel closed without `idle`, gap G13): mark the cycle
`interrupted`, notify the user, respawn on the next message with backoff
(1 s → 60 s), crash-loop breaker at `crash_max` crashes in `crash_window_secs`
(see `[worker_restart]` in `ka-agent.toml`).

The staged turn watchdog (`turn_watchdog.silent_secs` / `abort_secs`,
defaults 180 s / 900 s, either 0 = off) covers the wedge this table
cannot: a worker that is alive but emits nothing. Silence past the first
threshold notifies the operator once (`⏳ still working … /stop cancels`);
silence past the second aborts the turn exactly like `/stop` (the manual
`/retry` is the way back). A pending approval gate never triggers it.

## 8. Kill semantics and `EngineClosed` detection

- **Graceful stop:** `abort`, then wait `turn_grace_secs` (default 10 s,
  provisional) for `turn_finished{stop:"aborted"}` + `idle`.
- **Escalation:** if grace lapses (or the turn is still open), the runtime
  SIGKILLs the worker's **process group** directly (so tool children die
  too; the MVP engine layer sets `[tools.bash] background_after_ms = 0` so
  nothing detaches, gap G15). There is no intermediate SIGTERM step: the
  kill is immediate, bounded by a 3 s wait. The turn is marked `aborted`
  or `interrupted`.
- **EngineClosed detection:** the worker reports `EngineClosed{reason}` (see
  `worker-protocol.md`) and exits with code 3 when the engine channel closes
  without a terminal `idle`. The daemon treats any worker exit ≠ 0 without
  `idle` as a crash: mark `interrupted`, notify, backoff-respawn on the next
  message (never auto-replay the lost prompt; SEED §3.6).
