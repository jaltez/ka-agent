# Cron, @references, AGENTS.md, calendar digest (M11)

Four operator-facing features with one shared substrate: the tasks table
carries a JSON `payload` (schema v4), and the runtime scheduler fires both
reminders and generalized cron jobs.

## Cron

A cron job is a `kind = 'cron'` task whose `payload` is a `CronSpec`:

```json
{"text": "…", "source": "operator|kagent", "recurrence_ms": 3600000|null,
 "mode": {"mode": "agent"} | {"mode": "no_agent", "kind": "calendar"}}
```

- `agent` jobs submit `text` to the engine as a prompt when they fire (a
  normal turn: budget, approvals and audit all apply).
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

**Recurrence semantics**: fixed interval (`every N…`), not calendar
expressions. After a successful fire the next occurrence is computed by
skipping missed occupations (`while next <= now`), so a laptop that slept
for three days fires once, not 72 times. `pause` maps to the existing
`interrupted` status (operator-driven reconciliation), `resume` re-queues.

## @references

Any free-text message (console, Telegram, Matrix) can inline context with a
word starting with `@`:

| Form | Expansion |
| --- | --- |
| `@<path>` | file content (8 000-char cap), workspace-relative or absolute |
| `@<dir>` | shallow listing (depth 2, 50 entries, dirs get `/`) |
| `@turn:<prefix>` / `@t:<prefix>` | that turn's question and answer |
| `@https://…` | the URL body, under the `http_get` grant + SSRF rules |

The expansion replaces the token **before** `handle_message`, so it is
persisted in `turns.inbound_text` — auditable and searchable via FTS.
Failures are visible markers (`[ref @x: not found]`,
`[ref @url refused: no grant …]`), never a dropped message. Total
expansion budget: 24 000 chars, then `… [refs truncated]`. Words like
`bob@example.com` are not references (only a leading `@` counts).

## AGENTS.md

On every boot, the runtime writes `AGENTS.md` into the workspace **only if
absent**. An operator's or repo's own AGENTS.md is never touched. The file
is static: what ka-agent is, where memory lives, the operator commands, the
approval/budget precepts. A read-only workspace logs a warning and boots.

## Calendar digest

`/calendar add <url-or-path>` registers a `no_agent` cron (kind `calendar`)
that fires daily at 08:00 UTC and renders the ICS source's next 24 h as
text. `/calendar today` renders it on demand. No engine involved.

Minimal ICS support, documented limits:

- `VEVENT` with `SUMMARY`, `DTSTART`, `DTEND`; line unfolding per RFC 5545.
- Window `[now, now+24h)`, lines `HH:MM[-HH:MM] SUMMARY` (or `(all day)`).
- **No timezone conversion**: times are shown exactly as the ICS states
  them; a trailing `Z` is displayed (`08:30Z`).
- **No RRULE expansion**: recurring events are counted and noted
  (`N recurring event(s) not expanded`).
- URL sources need the `http:get:<host>` grant (plus `http:get:private` for
  private targets) exactly like the `http_get` tool; file paths are the
  operator's own machine, unrestricted (read-only).
