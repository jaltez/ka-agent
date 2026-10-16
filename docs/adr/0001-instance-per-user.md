# ADR-0001: one instance per operator

- Status: accepted (Phase 6, 2026-10-16)
- Deciders: Javier (operator), ka-agent
- Context: SEED.md §12-Q6 — instance-per-person vs a shared instance with
  per-user Ba; SEED.md §8.1 requires exactly one operator per instance until
  Phase 6 chooses.

## Context

Ka Agent's whole state plane is per-operator by construction: one session,
one memory root (`memory/` + compiled `MEMORY.md`), one grants store, one
audit chain, one identity set, one budget. Channels (Telegram, Matrix) are
inbound adapters for that one operator, not user partitions.

Phase 6 asks for multi-user support. Two designs were on the table:

1. **Instance-per-user** (the SEED's default): every operator runs their own
   daemon with their own data root. "Multi-user" = N instances plus
   cross-channel identity linking *within* each operator.
2. **Shared-instance multi-tenancy**: one daemon, per-user sessions, memory
   roots, grants, budgets and audit partitions, with a routing key on every
   store call.

## Decision

**Instance-per-user.** One data root is the isolation boundary; identity
linking (the `identities` table, §8.1) is how one operator reaches their
instance from several channels, and it is the *only* cross-channel concept
that exists. `ka-agent identity link matrix @javier:example.org` links a
second channel to the same operator.

## Why not shared tenancy

- **Everything would need a partition key.** The store, the runtime, the
  memory compiler, the scheduler, the plugin host and the audit chain all
  assume one operator. Keying them per user is a rewrite of every layer to
  reach parity, with fresh leak classes (a missed `WHERE user = ?` is a
  cross-person memory leak — exactly the Phase 6 risk named in the SEED).
- **The isolation argument collapses anyway.** Correct per-user isolation
  requires separate memory roots and separate engine sandboxes — which is
  what separate instances already are, minus a supervisor process.
- **No current use case.** The product is a personal resident assistant; the
  operator's household is served by N instances on one host (systemd
  template units, one per user, is the Ops milestone's shape).

## Consequences

- Isolation is structural: `nothing crosses instances` is enforced by
  separate data roots and proven by `isolation_e2e` (memory, history,
  grants, identities all empty in a second instance while the first one
  works).
- Cross-channel identity linking is per instance and CLI-only (`identity
  link|unlink|list`, audited as `identity.link`/`identity.unlink`).
- Deployment for several operators: one instance each (systemd
  `ka-agent@<user>.service` template), one data root each under their own
  `$HOME` or explicit `KA_AGENT_DATA_DIR`.
- Revisit trigger: a real product requirement for shared hosting (several
  operators on one daemon) — at that point this ADR is superseded by a
  multi-tenancy design with per-user store partitions and a threat model for
  the leak classes above.
