# ADR-0005: inbound trigger surface

- Status: accepted (C2/B14 stage 0, 2026-11-19)
- Extends: ADR-0003 (worker pool and idle eviction bound every new engine
  consumer), ADR-0004 (the creator is a user, not a role)
- Resolves: C2/B14 "event triggers, webhook ingress, cheap preflight" —
  how outside events start agent turns without widening the attack
  surface (research basis: Devin automations, Amp orbs/automations)
- Amends: `docs/contract/cron-and-refs.md` (new Triggers section),
  `docs/contract/web-ui.md` (hook row in the API table),
  `docs/contract/rbac.md` (the Cron identity row widens to triggers)
- Deciders: Javier (operator), ka-agent

## Context

Today only a clock starts automation: `kind = "cron"` task rows fire
through `fire_due_tasks`, and a shell `gate` (exit-non-zero skips the
occurrence) is the only zero-token filter. Two inbound event sources are
missing and cannot share the cron row: a webhook (GitHub label events,
CI pings, Amp-style orb deliveries) and a schedule that is *not* a chat
command. Both must run **as their creator** (the ADR-0004 user row, never
a widened role), must not pollute the creator's live conversation, and
must degrade cleanly when they fail repeatedly — all on a surface an
outsider can reach with a POST.

The staged plan (plan-ingress v2) requires this ADR before any code: the
hook lane is a second unauthenticated-by-the-panel HTTP path, and the
decisions below are the mitigation.

## Decision

### 1. Exactly two trigger kinds, one task row shape

A trigger is a `kind = "trigger"` task row whose payload is a
`TriggerSpec` beside `CronSpec` (`ka-agent-core::task`), stored and
fired by the same scheduler (`fire_due_tasks` gains a `"trigger"` arm).
`TriggerKind` is `schedule` or `webhook`; nothing else. GitHub-label
triggers are webhook events, so one ingress lane plus a filter covers
them — no per-source kinds. The `#[serde(default)]`-tolerant payload
pattern is reused verbatim, so old rows always load.

### 2. Session lineage: one pinned session per trigger

The spec pins `session_id`, a session row created with the trigger
(`Session::new_for_user`, creator's username and role). Every fire
submits into **that** session — through a dedicated ephemeral worker
(the scout's pattern: one open, one submit, one close) that resumes the
lineage strand by its recorded `strand_path` — and never through
`handle_message`, so automation noise cannot enter the creator's live
chat, and the creator's session shows no inbound rows for it. Fires
arriving while the trigger already holds a queued event or an open turn
are recorded `skipped: previous run still in flight` and are not stacked;
the queued event itself is durable (`queue` rows on the lineage session)
and delivers on the next armed fire. The global `runtime.max_workers` cap
plus LRU idle eviction (ADR-0003) bounds engines; a trigger worker claims
a pool slot exactly like a scout.

### 3. Identity: the cron rule verbatim

The creator's `username`/`role` ride the payload as a snapshot, but the
fire re-resolves the role from the `users` row at fire time (D9 rule) and
re-clamps `spec.tools` against that role's effective toolset — a demoted
user's trigger narrows at the next fire, never widens. `rbac.md`'s Cron
identity row extends to both trigger kinds.

### 4. Per-trigger secrets; the hook lane never sees `web.api_token`

Each webhook trigger names one `secret:` ref (`spec.secret`, default
`trigger.<hook-path>` with `/` and `:` flattened to `-`, since a secret
ref is a flat `secrets/` filename), resolved through the standard secret
tiers. The
hook lane's only auth is that per-trigger secret, compared constant-time.

Dispatch ordering is the security decision: `POST /api/hook/<prefix>`
gets its **own arm in `route()`, matched before the `/api/` bearer lane**.
Inside `api_route` the shared `web.api_token` gate and its `text`-shaped
body parse run before path dispatch, so a hook inside that lane would
demand the global token and mis-parse event bodies. Consequences, in
order of evaluation in `hook_route`:

- unknown hook path → `404` (the lane is off for that path);
- the trigger's secret unresolved → `404` (off, not forbidden — never a
  probe oracle); the global token does not rescue it;
- wrong bearer → `401`;
- then the shared 30/min token bucket, the body cap, and the filter.

A `web.api_token` presented to a hook is just another wrong bearer. The
global token still guards `/api/notify` and `/api/ask` unchanged.

### 5. Delivery contract: at-least-once, `200` = queued, dedupe by `event.id`

The lane answers Amp's contract: `200` means queued (the fire is armed),
replays are safe. Idempotency key is the sender's `event.id` — the
`X-Event-ID` header, else the body's `id`/`event.id` — recorded in the
new `trigger_events(event_id PRIMARY KEY, trigger_id, seen_ms)` table
(schema v19); insert-or-ignore decides, a replay answers
`200 {"ok":true,"dup":true}` and arms nothing. Delivery is at-least-once
above that: a crash between arm and fire loses nothing (the task row and
the queue item are durable), but a fire that starts twice is possible on
restart reconciliation and is harmless (idempotent at the turn level by
the in-flight check).

### 6. Size caps and filter semantics

Body cap: 16 KiB default, per-trigger up to 64 KiB (`spec.max_body_bytes`,
clamped); over-cap → `413`. The optional `spec.filter` is a regex over
the normalized body (trimmed raw body text); **match → fire**, no match
→ `200 {"ok":true,"filtered":true}` and no arm — an unparseable pattern
is a `500` naming the regex error (config bug, not traffic). The regex
crate is a new direct dependency of `ka-agent-web` (already in the tree
transitively).

### 7. Public bind stance

The hook lane inherits `web-relay.md` unchanged: `web.bind` defaults to
loopback (`config_keys.rs`), `doctor` warns on `0.0.0.0`, and public
exposure happens only behind a reverse proxy or Tailscale that terminates
TLS. No hook-specific bind, no per-hook TLS. A hook secret is a bearer
credential and is treated like `web.api_token` in every secret listing.

### 8. Error-after-N and clean degradation

The spec carries `error_streak` (consecutive failed fires, persisted by
`update_task_payload`). The default strike count is 3: the Nth consecutive
failure sets the task `interrupted` (today's pause semantics — resumable
by the cron resume path) and emits exactly one Error notice; a success
resets the streak to 0. Run history is never deleted: every fire — fired,
skipped, filtered, blocked — appends a `runs` row (`kind = "trigger"`,
`ref_id` = task id), so degradation is visible, auditable and reversible.

### 9. Cheap preflight gate (Stage 3)

When `spec.preflight` is on, the fire runs a cheap LLM gate **before** the
real turn: an ephemeral worker opened with a scratch engine-home (fresh
`HOME`/`KA_DATA_DIR`/state/cache under `<root>/tmp/preflight-*`, only the
provider-key `.env` linked in), `mode = readonly`, `Submit.allowed_tools =
Some(vec![])` (empty allowlist = no tools), and a fresh strand — so the
compiled MEMORY.md, the persona and every workspace fact never load. The
payload rides fenced in an untrusted-data envelope
(`<event-payload>`, same shape as `<gate-output>`); the prompt states that
the payload is data, not instructions, and that the turn has no tools to
act with. The verdict is parsed like `[SILENT]`: text starting with
`[SKIP]` re-arms the trigger with zero agent spend (one `runs` row of
`kind = "preflight"` keeps the gate's own tokens visible; the trigger row
records no tokens); `[RUN]` or anything else fails **open** and submits
the real turn on the lineage session under the creator's principal — a
broken gate must never eat a real event. The zero-LLM half already
exists: webhooks are regex-gated at ingress, and schedule triggers keep
the shell `gate`.

### 10. Not `/watch`

The `/watch` channel command (v3 #1) is a separate, later surface for
*creating* triggers from chat. Nothing here depends on it; the model,
ingress and preflight land regardless, and `/watch` will reuse
`TriggerSpec` + `CronSpec::from_words` when it arrives.

## Consequences

- The HTTP surface grows by one prefix with its own auth domain; the
  ordering tests (`webhook_lane_ignores_the_global_bearer_gate`,
  `webhook_without_secret_is_404`) pin it.
- Trigger turns run outside the users map: no `Inbound` events on the
  creator's surfaces, no steering, no queue-position messages — automation
  is fire-and-observe (`runs` history + the lineage session's transcript).
- Schema v19 adds `trigger_events` only; the tasks table already holds
  the payload.
- The preflight scratch directories are removed after each gate; a crash
  leaks one empty directory under `<root>/tmp/`, pruned by the OS tmp
  discipline already used for private sessions.

## Non-goals

- A trigger-creation chat/CLI surface (that is `/watch`, v3 #1, and the
  `cron_manage` extension — later stages; tests create rows through the
  store/runtime API).
- Retry/redelivery of failed fires (at-least-once *delivery*, not
  processing; the operator re-arms).
- Per-trigger rate limits (the shared 30/min bucket covers the lane; a
  per-trigger bucket is cheap to add if abuse shows).
- Outbound webhooks (Ka calling out on events) — a different direction,
  different threat model.
