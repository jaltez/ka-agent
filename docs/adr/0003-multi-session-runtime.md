# ADR-0003: multi-session runtime for a team

- Status: accepted (team edition P0, 2026-11-13)
- Supersedes: ADR-0002's single-session core (its per-subject partitioning
  survives, re-keyed from role to user)
- Related: ADR-0004 (the user model this runtime serves)
- Deciders: Javier (operator), ka-agent

## Context

ADR-0002 gave every linked identity a *role* and parked one role while
another was active: exactly one live engine, one `Inner`, a
`HashMap<String, ParkedRole>` and a `switch_role` that killed the worker and
respawned it on return. That is right for one operator with a guest. It is
wrong for a team: two users chatting at the same time queue behind each
other, and every role handover pays a worker spawn.

The team edition (D2: 5–15 users, D5: users and concurrency land as one
unit) needs N concurrent conversations on one daemon, one store, one bot
identity per channel and one audit chain. Per B5 in the roadmap, workplace
channels make one daemon per user impossible (one Slack app install serves
the whole workspace), so the concurrency has to happen in-process.

Constraints that do not move: one SQLite store with a single connection
behind a mutex; deliveries stay per-source (`Envelope.source`); approvals,
crash recovery and budgets keep their durable semantics; a user must never
see another user's conversation, memory, notes, strands or workspace.

## Decision

**A supervisor of per-user conversational state with a bounded pool of live
engines.**

1. `Inner` becomes `UserState`, one per username, held in
   `users: Mutex<HashMap<String, UserState>>`. A `UserState` carries what
   `Inner` carries today (session, session mode, session-allowed calls, live
   worker handle, budget window, crash history, restart attempt) plus the
   user's effective role and per-user effort. `parked` disappears: a user's
   state is simply its entry in the map, whether or not its worker is live.
2. `activate_user(source)` resolves source → identity → username and
   succeeds unless *that user's* session is `Running | AwaitingApproval |
   Stopping`. No global switch, no head-of-line blocking across users.
3. `ensure_worker` operates on the caller's `UserState`. The single-flight
   guard is per user; a global live-worker cap
   (`runtime.max_workers`, default 8, `Kind::Int` 1..=64, `Apply::Restart`)
   bounds total engines. Over the cap the scheduler evicts the
   least-recently-active *idle* worker (the existing idle-reap path:
   deliberate close, session persists, the strand reopens on the next
   message). If every live worker is mid-turn the message queues on that
   user's session and `drain_waiting_users` (round-robin over users with
   queued items and a free slot) hands over at the next
   `TurnFinished`/`EngineClosed` — a long turn never blocks a different
   user.
4. Idle eviction stays per worker (`worker_idle_timeout_secs`). Crash
   handling (`rt_on_engine_closed`), the crash-loop breaker and `drive`'s
   port-identity check (`Arc::ptr_eq`) move into the per-user state: a
   superseded worker's events only belong to the user whose port it was.
5. Worker spawn sets the user's partition root: `admin`-role users keep the
   profile directories, everyone else `<data>/users/<username>/…`
   (`roles/<r>` layout reborn per user). Memory stores are opened per user
   root; the compiled `MEMORY.md`/`IDENTITY.md` follow.
6. Budgets key on the user: `meta["budget:user:<username>"]` (the operator
   keeps the pre-RBAC `budget` key). `set_mode`'s clamp uses the caller's
   user role.
7. `status_report` grows a per-user section (status, live worker, queue
   depth, today's spend); the single-session fields stay for the operator.

The supervisor stays a supervisor: no new process model, no external
scheduler, no second store connection. Concurrency is a change of *how many*
engine subprocesses are alive and *whose* state each one serves, not a
change to the port or the engine.

## P0 spike — measurements

Method: `apps/ka-agent/tests/spike_concurrency.rs` (throwaway harness,
deleted after this record) spawned **N = 8** real workers through
`WorkerPort::open` with the scripted speaker, measured `open` latency
(process spawn + protocol `ready`) per worker and the steady-state RSS of
every worker process tree after 4 s idle. Same run repeated with the worker
under an engine bubblewrap profile:
`--ro-bind /usr`, `/lib*`, `/bin`, `/sbin`, `/etc` read-only, the user's
root bound read-write, `--dev /dev --proc /proc --tmpfs /tmp
--unshare-pid --die-with-parent`; **network shared** (engines need provider
egress, which is why the plugins sandbox — `--unshare-net` — cannot be
reused).

| Run | open min/med/max | steady RSS, 8 workers | per engine |
| --- | --- | --- | --- |
| bare | 3 / 3 / 5 ms | 228.0 MiB | 28.5 MiB |
| bwrap | 8 / 8 / 13 ms | 255.6 MiB | 31.9 MiB (+2 supervisors: 1.9 + 1.4 MiB) |

Debug build (`cargo test`), scripted speaker, no engine turn executed — the
numbers bound *process* cost, not inference cost. Repeated runs vary by
≤2 ms and ≤2 MiB; the ratios are stable.

Measured overhead of the sandbox: **startup 2.7× (3 → 8 ms)** and
**memory +12 %** (28.5 → 31.9 MiB per engine).

Operational facts recorded for sizing: 8 idle engines cost ~230 MiB before
the sandbox; the `max_workers = 8` default therefore fits a 1–2 GB VPS with
room for the engine's working set. `--new-session` is **not** usable in the
engine profile: the port already puts every worker in its own process group
and bwrap's `setsid()` then fails, killing the worker at startup.

## D11 gate — sandbox default

The gate set in the plan: enable `runtime.sandbox_engine = true` by default
only if startup overhead ≤ 2×, memory overhead ≤ 10 %, and the contract
suite passes under it. The spike **fails both numeric gates** (2.7× and
+12 %).

Decision: `runtime.sandbox_engine` **defaults to `false`**. The key ships,
the profile ships behind it, and `docs/contract/rbac.md` documents the
honest boundary: same UID, filesystem partitions + `__guard` + per-user
engine HOME, no OS-level isolation between team members. Revisit trigger:
a deployment with untrusted members, or a release build re-measured where
both gates pass (the relative overhead grows in release, where the worker
baseline is smaller, so this is not expected).

## Consequences

- Two users' turns run in parallel; the pilot's exit test asserts two
  overlapping in-flight turns.
- Memory cost is bounded by `max_workers`, not by the number of accounts
  (idle workers are reaped; a queued user waits for a slot, never for
  another user's whole turn).
- Crash isolation is per user: one crashing engine degrades one user's
  session; the others keep running. The crash-loop breaker keeps counting
  per user.
- A role rename no longer exists as a runtime concept; the runtime resolves
  users. Telegram/Matrix/console behavior for a single-user install is
  unchanged (one user `operator`, its own session adopted at boot).
- Store additions this forces: `sessions.username` (replacing the role
  partition key), `memory_candidates.username`, `audit.username`,
  `turn.username` meta key beside `turn.role`, and user-scoped siblings of
  the role-scoped queries (`latest_session_for_user`, …).

## Non-goals

- OS users or VMs per team member (D11 chose bubblewrap; it did not pass).
- A second store connection or a writer task (the single connection +
  `busy_timeout` is kept; revisit only if the concurrency e2e shows lock
  contention).
- Horizontal scaling or stateless replicas (roadmap §deferred).
- Pre-emptive migration of in-flight work between workers: a user's turn
  stays on the worker that started it.
