# ADR-0002: in-process RBAC for linked identities

- Status: accepted (2026-11, v0.13)
- Supersedes: ADR-0001 (one instance per operator) for multi-subject use
- Deciders: Javier (operator), ka-agent

## Context

ADR-0001 made the data root the isolation boundary: one daemon, one
operator, every linked identity is that operator. That left no way to let a
guest, a family member or a work assistant talk to the same bot without
handing them the whole instance — the operator's session, memory, notes,
grants, budget and toolset.

Two designs were available:

1. **One daemon per subject** (ADR-0001's answer): N processes, N bot
   tokens, N data roots.
2. **In-process RBAC**: one daemon, one bot token, one profile/store; every
   inbound identity carries a **role**, and the role partitions the
   conversation and its capabilities.

## Decision

**In-process RBAC.** One daemon and one store; `[[role]]` entries in
`ka-agent.toml` define roles; `identities.role` binds each linked identity
to one. The implicit role `admin` (terminal, web panel, CLI, local API)
always exists and keeps the profile's own layout untouched. A subject's
role partitions:

| Surface | Admin | Role R |
| --- | --- | --- |
| Session | profile session | its own session row (`sessions.role`) |
| Memory | `<data>/memory` | `<data>/roles/<r>/memory` |
| Notes | `<data>/notes` | `<data>/roles/<r>/notes` |
| Workspace | `workspace` config | role `workspace` or `<data>/roles/<r>/workspace` |
| Engine HOME/state | `<data>/engine-*` | `<data>/roles/<r>/engine-*` |
| Grants | `[grants]` | `[[role.grants.path]]` only |
| Toolset | `[tools] disable` | role `allow` or `disable` (on top of the baseline) |
| Mode | `mode` config | role `mode` (ceiling and default) |
| Budget | `[budgets]` | `budget_daily_usd` when set, else the profile caps |
| Commands | all | role `commands` list plus the `help`/`status` floor |
| Approvals | decides | never (asks surface to admin) |
| Audit | `role = admin` | `role = <r>` on every row |
| Cron jobs | role `admin` | creator's role, toolset re-clamped at fire |

### Leak classes closed

- **Shared session** → session-per-role: one active session at a time, the
  others parked whole (session, session allows, budget, crash history,
  effort). A message from another role never aborts a running turn; it
  waits on that role's durable queue and the core is handed over at the
  next idle boundary. A superseded worker's events are dropped by port
  identity.
- **Memory contamination** → disjoint role roots; every memory mutation
  targets the active role's store, and an accepted candidate lands in the
  proposing role's root.
- **History/notes** → FTS search and `/history` are scoped to the role's
  sessions; `notes_*` resolve the turn's role.
- **Approvals** → cards are delivered to admin surfaces only; a subject's
  decision is refused and audited.
- **Toolset** → subtractive engine specs at spawn (fail-closed in the
  engine) plus a per-turn allowlist at admit; a cron may only narrow.
- **Mode** → a role's mode is a ceiling: `/mode` above it is clamped, and
  the worker re-asserts the role's mode after strand replay.
- **Audit** → every row carries the acting role (outside the hash chain,
  so pre-RBAC rows keep verifying).

### What stays shared

One bot token, one web panel (loopback, one operator), one config, one
secret store, one OS user, one store and one audit chain. Roles bound
*subjects of the assistant*, not hostile tenants.

## Non-goals

- **OS-level isolation between roles.** Every role runs as the same UID;
  an admin-mode engine can still read the OS home, and bash is not
  path-sandboxed beyond the guard. RBAC bounds subjects, not a malicious
  admin.
- Per-role models or provider keys (one `.env`; role workers read it
  through a symlink).
- Per-role web-panel accounts (the panel is loopback + the operator).

## Consequences

- `identities` gains `role`; `sessions`, `memory_candidates` and `audit`
  gain `role`. Schema v11 backfills `admin`, so an existing data root
  behaves exactly as before.
- `ka-agent identity link … --role <r>` and `identity role` manage the
  binding; a role absent from the config fails the command naming the
  config file.
- An identity whose role was removed from the config fails closed
  (read-only, engine-muted) instead of escalating to admin.
- The daemon does more work per switch (kill + respawn the worker); the
  single-session core stays, which is the price of one engine at a time.
