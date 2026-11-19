# Plans contract — editable plan artifact + verify/repair cycle (v4 F12)

Status: implemented (early slice), 2026-11-19, verified by e2e
(`apps/ka-agent/tests/plans_e2e.rs`, scripted engine through the real
binary) and store unit tests. The artifact is one SQLite row per plan
(schema v18); the cycle boundary logic lives in the `Idle` handler
(`runtime/events.rs`), the store surface in `store/src/plans.rs`, the
operator surface in `/plan` (`commands/plans.rs`) and the model surface
in the `plan_update` bridge tool.

## Artifact

`plans(id, session, username, title, body, verify, max_iterations,
iterations, status, draft_turn, created_ms, updated_ms)` — one plan per
draft/run cycle, owned by its `username` (`session` is provenance: id
resolution and `/plan list` are per user, so a plan survives session
rotation).

## Status machine

```
drafting --capture--> draft --approve--> approved --run--> executing
   |                    ^  |                |              |  |
   |                    |  +---- /plan set --+              |  +-- verify pass / gateless clean --> done
   |                    +------- /plan set (un-settles)       +-- repair cap reached -------------> failed
   +-- /plan stop --> abandoned <----- /plan stop (any live status) -+
```

- `drafting` — the `/plan draft` turn is running; the answer is not
  captured yet. The empty-answer nudge's re-ask appends; a still-empty
  cycle stays `drafting` (nudge gives up with its own notice).
- `draft` — captured body, `Verify:` line lifted into `verify`. Edits via
  `plan_update` (model) or `/plan set` (operator).
- `approved` — operator-only (`/plan approve`, admin role). **Approval
  arms the gate**: consent to exactly the lifted `Verify:` string, shown
  unarmed by `/plan show`. The model's `plan_update` is refused — an
  approved plan is immutable to the model.
- `executing` — the `/plan run` turn (or its repairs) is in flight. The
  body freezes (`plan_update` and `/plan set` refuse). Boot resumes
  nothing: `/plan run` on an `executing` plan re-submits the run turn in
  place (no state change) — the documented recovery after a restart or a
  budget-deferred repair.
- `done | failed | abandoned` — settled. `/plan set` un-settles any
  settled status back to `draft` (fix the body or the verify command,
  re-approve, re-run).

Illegal edges are refused by the store
(`StoreError::IllegalTransition` names the actual `from -> to`).

## Verify/repair cycle (daemon-side, in `Idle`)

For a turn whose source is `plan:<id>` on an `executing` plan, when the
cycle boundary is reached (`!ask_pending && !turn_open`):

- **Gate-eligible turn** (any stop except `error`/`aborted`/`interrupted`
  — `empty` is a done-family stop: the model said nothing, the gate is
  still the authority on done-ness):
  - `verify = NULL` → settles `done` (gateless approval runs on trust of
    the clean turn).
  - `verify` set → the zero-token gate runs (`sh -c`, session workspace,
    30 s kill, 4 KiB output cap, `spawn_blocking` — the `drive` task
    never blocks). **Exit 0 → `done` + Notice.** Non-zero (or spawn
    error / timeout) → one repair: while `iterations < max_iterations`,
    `bump_plan_iteration` and enqueue exactly one `QueueItem` (source
    `plan:<id>`) whose prompt carries the plan body and the gate output
    in the existing `<gate-output>` envelope — the ordinary queue drain
    submits it, single-writer untouched. At the cap → `failed` + Notice.
  - Budget-gated: a `BudgetVerdict::Exceeded` window defers the repair —
    the plan stays `executing` (re-run `/plan run` or wait for the
    window to roll; `iterations` is durable).
- **Unclean turn** (`error`/`aborted`/`interrupted`): runs no gate and
  auto-submits nothing (SEED §7.3) — the plan stays `executing`.
- **Empty-answer interplay**: a `drafting` plan keeps the standard
  empty-answer nudge (its capture has no other retry; a still-empty
  re-ask stays `drafting` under the nudge's own give-up notice). An
  `executing` plan's empty answer is the verify cycle's problem — the
  nudge stands down so the failure is never double-fed.
- **Stop/restart**: `/plan stop` → `abandoned` + `queue_remove` of the
  pending repair; boot finding `executing` with an empty queue resumes
  nothing (the operator re-runs `/plan run`; the empty-answer nudge is
  the only automatic re-ask, bounded to one).

Every gate execution appends an audit row, action `plan.verify`
(`ka_agent_core::audit::actions::PLAN_VERIFY`), detail `pass|fail; repair
n/max; exit <code>` — a local shell, cron-gate parity; README's intents
rule stays scoped to external writes.

## Security line

- `verify` executes only under an `approved` plan; `approve` is
  operator-only (admin role); `/plan show` renders the command *unarmed
  until approve*; approval consents to exactly that string. The gate
  runs as the daemon in the session workspace, like a cron gate.
- The model writes plan bodies only through `plan_update`, which refuses
  approved rows and rows owned by another user; the operator's `/plan
  set` is the only path that un-approves.
- `plan:<id>` boundary effects apply only to the plan's owner — a repair
  or capture never lands on another user's artifact.

## Commands (`/plan <verb>`, shared router)

- `/plan draft <goal>` — insert a `drafting` row, submit the draft turn
  (source `plan:<id>`); the answer becomes the body at the boundary.
- `/plan list` — the caller's plans (id, status, repairs, title).
- `/plan show <id>` — body + status + `verify` (unarmed until approve).
- `/plan set <id> <markdown>` — rewrite the body; un-settles to `draft`.
- `/plan approve <id>` — operator-only; arms the gate.
- `/plan run <id>` — `approved → executing`, submit the run turn.
- `/plan stop <id>` — `abandoned` + drop the queued repair.

## Tool (`plan_update`, `Effect::LocalWrite`, auto-allowed)

`plan_update{id?, body}` — rewrite the body of a `drafting`/`draft` plan
(id optional when the call runs inside a `plan:<id>` turn). Anything
else (approved, executing, foreign user, empty body) is an isError.

## Non-goals (this slice)

B5 stop-gate hook and B6 same-turn re-feed live below the port; debug-
with-rollback waits on B1 checkpoints (`/rewind` is the re-propose UX
reference). The iteration cap is the only bound here.
