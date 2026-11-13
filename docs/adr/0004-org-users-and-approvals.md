# ADR-0004: organization, users and approval routing

- Status: accepted (team edition P0, 2026-11-13)
- Extends: ADR-0002 (role partitions become user partitions)
- Resolves: ADR-0001's revisit trigger for the team track (B5) — one daemon
  serves one *organization*, not one tenant per customer
- Amends: `docs/contract/rbac.md` ("approvals are decided by the admin
  role"), `docs/contract/web-ui.md` (pairing codes → accounts)
- Deciders: Javier (operator), ka-agent

## Context

ka-agent's subject model is a *role*: a linked channel identity carries a
role, and the role partitions session, memory, notes, workspace, engine
HOME, grants, toolset, mode ceiling, budget, commands, cron and the audit
stamp. Roles are permission sets; they are not people. There is no place to
put "Ana is on holiday", "the org pays but Ana has $2/day", "this document
is for the dev group", or "who approved this and on whose authority".

The team edition (D1–D13 in `docs/design/team-edition-roadmap.md` §1.1)
serves **one organization of 5–15 people** on one VPS with its own domain.
The identity chain therefore becomes three links, not two:

```
channel identity  ->  user (a person)  ->  role (a permission set)
```

and the partition key moves from role to user. Roles keep their existing
meaning (toolset, mode ceiling, grants, commands, budget cap) and gain a
membership layer (`groups`) that governs *resources*: which models a person
may use and which knowledge bases they can read.

The decisions taken in the interview that this ADR implements:

| # | Decision | Landed here as |
| --- | --- | --- |
| D7 | The org pays; per-user/group quotas | shared key set, `users.quota_daily_usd` overrides the role/profile budget, model allowlists per group |
| D8 | Local accounts (argon2), admin-minted invites, admin resets | `web_sessions` table, `invites` table, `/login` + `/invite` |
| D9 | Policy-based approval routing | self-scope vs admin escalation (below) |
| D10 | Offboarding pauses everything | `user disable/enable/export/purge` |
| D12 | Retention configurable, default keep-everything | `retention.turns_days` / `retention.attachments_days` |
| D6 | KBs with per-group access, no per-document ACL sync | `kbs`, `kb_docs`, `kb_access`, `kb_sources` (sync connectors) |

## Decision

### 1. Users are the partition key; roles stay permission sets

New tables (schema v12, same SQLite file):

```sql
users(username, display_name, pw_hash, role, quota_daily_usd, disabled,
      created_ms, created_by)
groups(name, models_allow, models_default)
users_groups(username, group_name)
```

`users.username` is the primary key; `users.role` names a `[[role]]` in the
config (`admin` allowed and is the operator's own). `pw_hash` is an argon2id
PHC string, `NULL` until the first login sets it (invite redemption).
`quota_daily_usd` overrides the role/profile budget when set.

Backfill: user `operator` (role `admin`) is created; every existing
identity with role `admin` binds to it; each distinct non-admin role that
has at least one identity creates a user named after the role holding that
role. Existing `sessions`, `audit` and `memory_candidates` rows backfill
`username` from their `role` by the same mapping. An existing single-user
install therefore ends with exactly one user `operator` and identical
behavior.

`identities` keeps `role` (a role can be reassigned per identity today) but
gains `username`: the runtime resolves source → identity → **user**, and the
user's role is authoritative. `identities.role` remains the pre-migration
source for the mapping and is what `identity link --role` still writes for
compatibility; the runtime never reads it when a username is bound.

### 2. Approval routing (D9) — amends the RBAC contract

Today `docs/contract/rbac.md` says approvals are decided by the admin role.
For a team that is unusable: every ordinary file write inside a user's own
grants would need the admin's click.

An intent raised in user U's turn is **self-scope** iff all hold:

- (a) the affected path/resource is inside U's role `[[role.grants.path]]`
  set;
- (b) the projected cost fits U's remaining daily quota;
- (c) the tool does not match U's role `escalate` list (new optional `[role]`
  key, same spec grammar as `allow`: engine hand names and
  `mcp:<server>[:<tool>]`; default empty).

Self-scope intents surface as approval cards on **U's own surfaces** (web
chat inline, Slack/Discord/Telegram buttons) and U's `/y`/`/n` decide them.
Everything else — shared resources (KB writes, org config, agent edits),
over-quota actions, `escalate` hits — goes to the admin queue exactly as
today, with the escalation reason on the card. Cron-created intents carry
the creator's username and route by the same rule.

At execute time every approved intent re-resolves actor state: user
disabled, role removed, or grant withdrawn ⇒ deny + admin notice + audit
`approval.stale`. Over-quota always escalates regardless of lists.

The approval row gains `username` and `escalated` (0/1). The admin queue is
"pending approvals with `escalated = 1` or no username". Authorization in
`Runtime::decide`: admin always; the acting user iff
`approval.username == theirs && escalated = 0`; everyone else refused with
the existing wording plus an audit row.

### 3. Groups govern resources

A user's effective model policy is the union of `models_allow` across their
groups (absent or empty ⇒ all catalog models); the default model is the
first non-NULL `models_default` scanning the user's groups in alphabetical
order, else the profile default. Enforcement points: `Runtime::set_model`,
the `/model` listings (filtered), submit-time resolution (a non-allowed model
is a named refusal, never a silent fallback) and the web pickers.

A user's effective KB set is: KBs whose `kb_access` intersects the user's
groups, plus KBs the user owns, plus (role `admin`) all KBs.

### 4. Accounts (D8)

`web_sessions(token_hash, username, created_ms, expires_ms)` replaces the
in-memory `Auth.sessions` map: sessions survive a restart and are swept on
access. The cookie stays `ka_session`, `HttpOnly; SameSite=Strict` (+
`Secure` under an https `public_url`). `/login` takes username + password
(argon2id verify; 5 failed verifies per username per 15 minutes ⇒ lockout
with an audit row). `/invite` takes an invite code and a new password.

Invites are admin-minted (`ka-agent user invite [--role] [--group]…`), 24 h
TTL, 5 attempts, constant-time compare, row in `invites` (multiple
outstanding — unlike the single `pairing.code` meta slot). Redemption sets
`pw_hash`, marks `used_ms`, binds the groups.

`POST /pair`, `POST /pair/issue` and `ka-agent pair web --role` are
**removed** (clean cutover; Telegram/Matrix pairing is unchanged). The
`X-Ka-Agent: 1` header requirement extends from the fetch endpoints to
**every mutating form POST**.

Role is re-derived from the identity at turn time, never trusted from the
captured session. `Principal::User { source, username, role }` replaces
`Principal::Role`; admin users see the full panel, everyone else the
chat-only allowlist — one login, reach derived from the user's role.

### 5. Offboarding (D10)

`ka-agent user disable <name>` sets `disabled = 1`, deletes the user's
`web_sessions`, treats their identities as unlinked at resolution time (an
inbound message is refused exactly like an unknown sender: no reply, one
`<channel>.unauthorized` audit row surfaced on `/contacts`), pauses their
crons through the existing pause machinery and audits `user.disable`.
`enable` reverses all four. `user export` writes a tar of the user's
session/memory/notes/attachment trees; `user purge` deletes them and
requires `--yes` (warns when no export exists). Nothing is deleted
implicitly.

### 6. Retention (D12)

`retention.turns_days` and `retention.attachments_days` extend the existing
`Store::prune` job (no new scheduler): 0 = keep forever, the default. The
audit chain is never pruned. Exports exist so "purge" is a deliberate act.

## Consequences

- `slug`: every session row, audit row, memory candidate and turn context
  carries the acting username (`turn.username` meta key beside
  `turn.role`); `/history`, memory review and cron listings are scoped to
  the caller's user.
- User partitions are `<data>/users/<username>/…`; the operator keeps the
  profile directories. Existing `<data>/roles/<r>` trees move to
  `<data>/users/<r>` on first start (idempotent: only when the source
  exists and the target does not).
- One daemon, one store, one bot token per channel, one audit chain — the
  org is the trust boundary (OpenClaw's frame), not each user.
- Roles stay the permission-set layer, so every existing `[[role]]` key
  keeps working; a team config is roles + users + groups, not a rewrite.
- Failure mode of an unlinked or disabled identity is silence + an audit
  row, never a guess.

## Non-goals

- Multi-tenant SaaS (one organization per install; roadmap §1).
- SSO/OIDC/SAML/SCIM, MFA (local accounts + invites cover ≤15 people; D8).
- Email infrastructure of any kind (no invites by mail, no password reset
  mail: the admin resets by clearing `pw_hash` and minting a new invite).
- Per-document ACLs or ACL sync from knowledge sources (D6).
- OS-level isolation between users (ADR-0003's sandbox gate failed; the
  same-UID boundary is documented in `rbac.md`).
