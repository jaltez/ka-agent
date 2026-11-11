# RBAC contract — roles for linked identities

Status: implemented 2026-11 (ADR-0002). One daemon, one profile; every
linked identity acts as a **role**, and the role partitions its session,
memory, workspace, grants, toolset, budget and command surface. The
terminal, web panel, CLI and local API are the implicit `admin` role.

## Config

```toml
# Per-profile engine baseline (subtractive, also the admin's toolset).
[tools]
disable = ["tool:bash", "mcp:github"]

[[role]]
name = "guest"                 # [a-z0-9-]{1,32}, unique, never "admin"
mode = "readonly"              # ask|accept_edits|allow|readonly (aliases work)
allow = ["read", "grep", "kagent__notes_search"]  # per-turn allowlist
# disable = ["tool:bash"]      # OR subtractive specs — never both
# workspace = "~/projects/shared"   # default <data>/roles/<name>/workspace
# budget_daily_usd = 2.0       # unset = the profile's [budgets] apply
# commands = ["status", "help", "skills", "skill", "history", "memory"]

[[role.grants.path]]           # role sessions see ONLY these grants
dir = "/srv/reports"
access = "allow"
```

Rules (all hard config errors):

- `name`: `[a-z0-9-]{1,32}`, unique; `admin` is reserved (`reserved role
  name "admin" — it is implicit`).
- `allow` XOR `disable`. `allow` entries are exact engine hand names;
  `disable` entries use the engine's `[features] disable` grammar and are
  validated with it before any worker spawns.
- `mode` is both the role's default and its **ceiling**.
- `workspace`: absolute or `~`-relative; it may not nest with the profile
  workspace, ka-agent's own tree (`roles/` included), or another role's
  workspace.
- `budget_daily_usd > 0` when set; `commands` entries non-empty.
- `[[role.grants.path]]`: absolute and unique; `install`, `full` and
  `advisory` are admin-only and refused on a role.

## Identity binding

```sh
ka-agent identity link telegram 12345 --role guest   # default: admin
ka-agent identity role telegram 12345 guest          # reassign (audited)
ka-agent identity list                               # prints the role
```

A role not defined in the active profile's config fails the command naming
the config file. An identity whose role is later removed fails closed
(read-only, engine-muted). Identity changes are audited with the new role
and `identity.role` / `identity.link` actions.

## Enforcement points

| Layer | Mechanism | Failure mode |
| --- | --- | --- |
| Spawn | `[features] disable` = `[tools]` + role `disable`, merged under the operator layer, cross-checked with the engine parser | worker refuses to spawn, spec named |
| Turn | `Submit.allowed_tools` = role `allow` (or cron `tools` ∩ role allow) | tool filtered from the model specs and rejected at admit |
| Mode | role `mode` ceiling; worker re-asserts it after strand replay | a looser `/mode` clamps to the ceiling |
| Commands | role `commands` + `help`/`status` floor; `update`, `keys`, `quit`, `persona`, `model`, `config` are admin-only whatever the list says | one-line refusal |
| Files | `__guard`: every other role root and the profile tree are write-protected; role reads of the profile's memory/notes/strands/store are refused | tool call denied |
| Memory | disjoint roots; accepted candidates land in the proposing role's root | n/a (routing) |
| Notes | `turn.role` store key → `<data>/roles/<r>/notes` | absent key = admin (in-flight turn) |
| History | FTS/`/history` scoped to the role's sessions | other turns are invisible |
| Approvals | cards delivered to admin surfaces; a subject's decision is refused and audited | `approvals are decided by the admin role` |
| Budget | role USD cap (else profile) on the role's own state; `meta["budget:<role>"]` | the role pauses; admin unaffected |
| Audit | every row carries the acting role | n/a (stamped) |
| Cron | creator role in the payload; tools re-clamped at every fire; `/cron list`, `/reminders` and `/cron rm` are scoped to the caller's role | a subject never sees or cancels the operator's jobs |

## Web relay and browser sessions

- `ka-agent pair web --role <r>` prints a one-time code (10 minutes, five
  attempts, constant-time compare). `POST /pair` on the web panel redeems
  it, links a fresh `web:<id>` identity with that role, and returns an
  `HttpOnly; SameSite=Strict` session cookie.
- A role cookie reaches `/` (its own chat, its own history), `/chat/send`
  and `/events` only. `/events` is filtered server-side: a role stream
  carries envelopes stamped with its own `web:<id>` source, never
  admin-only events or another role's turns.
- Every admin page, config mutation, approval decision and profile op
  stays behind the operator's login (`/auth` one-shot token). A role
  session gets `403 this web session is chat-only`.
- The cookie is server-side state; losing it means pairing again. The
  daemon never trusts a role asserted by the client.

## Delivery

Turn-scoped events carry their source (`telegram:<uid>`, `matrix:@user:domain`,
`console`, …). Telegram keeps one chat per sender and Matrix one room per
sender; admin-only events (approval cards, memory proposals) go to the
admin's chat/room. A message for another role while a turn runs is queued
on that role's session, never steered into or submitted over the live one.

## What roles do not get

- Their own model, provider keys, bot token or web-panel account.
- Approval authority, config/keys/persona/model commands.
- OS-level isolation from the operator (same UID; see ADR-0002).
