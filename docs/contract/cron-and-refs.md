# Cron, @references, AGENTS.md, calendar digest (M11)

Four operator-facing features with one shared substrate: the tasks table
carries a JSON `payload` (schema v4), and the runtime scheduler fires both
reminders and generalized cron jobs. Reminders (`kind = 'reminder'`) are
notifications: on fire, the surface gets `⏰ <text>` and no engine turn is
spent. The maintenance loop sleeps until the next due task (bounded by a
15 s interval) and wakes immediately when the daemon itself schedules
something (`scheduler_tick`).

## Cron

A cron job is a `kind = 'cron'` task whose `payload` is a `CronSpec`:

```json
{"text": "…", "source": "operator|kagent", "recurrence_ms": 3600000|null,
 "mode": {"mode": "agent"} | {"mode": "no_agent", "kind": "calendar"}}
```

- `agent` jobs submit `text` to the engine as a prompt when they fire (a
  normal turn: budget, approvals and audit all apply).
- A cron job runs the `text` snapshotted at creation; daemon upgrades
  never rewrite what an existing job asks the model. The daily briefing
  (`source = "briefing"`) bakes its full prompt into `text` when enabled
  and only time-derived `briefing_context` is added at fire time; legacy
  placeholder payloads were snapshotted once at boot (B13, logged one
  line).
- `no_agent` jobs dispatch to a runtime-registered handler by `kind`
  (today: `calendar`). No engine, no LLM. A missing handler or a failing
  handler marks the job `failed` — visible on the surface, never re-armed.

**Surfaces** (same grammar everywhere):

- Chat/console: `/cron [list]` · `/cron add in <Ns|Nm|Nh|Nd> <text>`
  (one-shot) · `/cron add every <Ns|Nm|Nh|Nd> <text>` (recurring) ·
  `/cron rm <id-prefix>`.
- Engine (MCP bridge): the `cron_manage` tool —
  `{"action": "schedule", "text", "in"|"every"}` and
  `list|pause|resume|remove|trigger` by `id` prefix. The bridge writes the
  store directly (SQLite WAL); it only ever touches `kind='cron'` rows,
  never reminders.

**Recurrence semantics**: `every N…` interval (model jobs: ≥ 60 s), or
operator `every day at HH:MM` / `every <weekday> at HH:MM` in the
configured local timezone — no raw cron expressions. Durations are capped
at 366 days (`parse_duration` refuses
more; `now + ms` can never wrap i64). After a successful fire the next
occurrence is computed by skipping missed occupations (`while next <= now`,
overflow-checked), so a laptop that slept for three days fires once, not 72
times; an unrepresentable schedule fails visibly instead of spinning.

**Late and missed fires (SEED.md §7.2)**: a due fire that sat overdue still
fires within the grace hour — with the lateness on the notification
(`⏰ stretch (late by 5m)` / `cron: … (late by 40m)`); lateness under a
minute goes unmarked. Overdue beyond that hour, one-shots are recorded
`missed` (a terminal task status, off the live set) and reported
(`⏰ missed: … (overdue by 2h)`); recurring jobs keep their schedule — the
missed occupation is skipped with a note and the job re-arms strictly
after now. Times render via `ka_agent_core::fmt_interval` (truncate to
the largest unit).

**Races and restarts**: firing claims the row `queued → running`
conditionally — a cancel or pause that lands first or mid-flight wins and
the fire loop skips or leaves the row alone (never resurrects a cancelled
job). A crash mid-fire leaves the row `running`; the next boot reconciles
it to `interrupted`, shown as `[paused]` and resumable. `pause` maps to
the existing `interrupted` status (operator-driven reconciliation),
`resume` re-queues.

## @references

Any free-text message (console, Telegram, Matrix) can inline context with a
word starting with `@`:

| Form | Expansion |
| --- | --- |
| `@<path>` | file content (8 000-char cap, read is byte-bounded at the source), workspace-relative or absolute |
| `@<dir>` | shallow listing (depth 2, 50 entries, dirs get `/`) |
| `@turn:<prefix>` / `@t:<prefix>` | that turn's question and answer |
| `@session:<prefix>` / `@s:<prefix>` | that conversation's header (short id, owner, created date, turn count) plus its last three exchanges, each citing its turn id |
| `@https://…` | the URL body, under the `http_get` grant + SSRF rules |

The expansion replaces the token **before** `handle_message`, so it is
persisted in `turns.inbound_text` — auditable and searchable via FTS.
Failures are visible markers (`[ref @x: not found]`,
`[ref @url refused: no grant …]`), never a dropped message. Total
expansion budget: 24 000 chars, then `… [refs truncated]`. Words like
`bob@example.com` are not references (only a leading `@` counts).

`@turn:` and `@session:` honor `rbac.md`'s History row: a non-admin caller
only ever resolves ids of their own partition — another user's turn or
session renders the same `[ref …: not found]` marker as a missing one (no
existence leak); the operator's history view keeps seeing everything.

## AGENTS.md

On every boot, the runtime writes `AGENTS.md` into the workspace **only if
absent**. An operator's or repo's own AGENTS.md is never touched. The file
is static: what ka-agent is, where memory lives, the operator commands, the
approval/budget precepts. A read-only workspace logs a warning and boots.

## Calendar digest

`/calendar add <url-or-path>` registers a `no_agent` cron (kind `calendar`)
that fires daily at 08:00 UTC and renders the ICS source's next 24 h as
text. `/calendar today` renders it on demand. No engine involved.

ICS support:

- `VEVENT` with `SUMMARY`, `DTSTART`, `DTEND`, `RRULE`, `EXDATE`; line
  unfolding per RFC 5545.
- Window: the operator's local day `[today 00:00, tomorrow 00:00)`; lines
  `HH:MM[-HH:MM] SUMMARY` (or `(all day)`), sorted by start.
- Times: `…Z` is UTC, `TZID=Area/City` is that zone, a bare time is
  floating (the operator's zone); every time is shown in the operator's
  zone.
- Recurrence: `FREQ=DAILY|WEEKLY|MONTHLY` with `INTERVAL`, `BYDAY` (weekly,
  plain day names), `COUNT`, `UNTIL` is expanded inside the window, stepped
  in the event's own zone (DST-correct); `EXDATE` occurrences are skipped.
  Other rules (positional `BYDAY=1MO`, `BYSETPOS`, yearly…) are noted as
  `N recurring event(s) with rules this digest does not expand`.
- URL sources need the `http:get:<host>` grant (plus `http:get:private` for
  private targets) exactly like the `http_get` tool; file paths are the
  operator's own machine, unrestricted (read-only).

## Triggers (ADR-0005)

An inbound trigger is a `kind = "trigger"` task row whose payload is a
`TriggerSpec` (beside `CronSpec`), fired by the same scheduler. Exactly two
kinds:

- `schedule` — the cron grammar (`when.rs`: `every day at 09:00`, fixed
  intervals, one-shots) with the optional zero-token shell `gate`; quiet
  hours defer a scheduled fire exactly like a cron's.
- `webhook` — fires when a delivery to `POST /api/hook/<hook-path>` is
  accepted (see `web-ui.md`'s hook row): per-trigger secret as the only
  auth, 16 KiB default / 64 KiB max body cap, optional regex `filter`
  over the normalized body (match → fire; no match answers
  `{"ok":true,"filtered":true}` and arms nothing), idempotency by the
  sender's `event.id` (`X-Event-ID` header, else the body's
  `id`/`event.id`) in the `trigger_events` table — replays answer
  `{"ok":true,"dup":true}`.

Both kinds run as their creator, under the cron identity rule verbatim:
the role is re-resolved from the user row at fire time and `tools` are
re-clamped against it — a demoted user's trigger narrows at the next fire,
never widens. Every fire submits into the trigger's pinned lineage
session (one session row per trigger, strand resumed across fires), never
the creator's live chat; the turn's source is `trigger:<task id>`.

Degradation contract: every fire appends a `runs` row
(`kind = "trigger"`, `ref_id` = task id; the preflight gate adds
`kind = "preflight"` rows for its own spend). Consecutive failed fires
count on the spec (`error_streak`); at 3 (`TriggerSpec::ERROR_STRIKES`)
the task pauses as `interrupted` with exactly one Error notice
(`ka-agent cron resume <short>` re-arms it — a webhook delivers its
queued event then); a success resets the streak. Per-trigger in-flight
cap 1: a fire while the lineage session still holds a queued or running
turn records `skipped: previous run still in flight` and does not stack;
a *delivered event* is durable (`queue` rows on the lineage session) and
delivers on the next armed fire. The optional preflight gate (cheap LLM
triage, no memory/tools, scratch engine-home) answers `[SKIP]`/`[RUN]`
and fails open.
