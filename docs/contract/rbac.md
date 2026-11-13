# RBAC contract — roles for linked identities

Status: implemented 2026-11 (ADR-0002); extended 2026-11 for the team
edition (ADR-0004): a **user** is a person, a **role** is the permission set
the user holds, and the partition key is the user. One daemon, one profile;
every linked identity resolves to a user, and the user's role partitions its
session, memory, workspace, grants, toolset, budget and command surface. The
terminal, web panel, CLI and local API act as the operator user (role
`admin`).

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
# workspace = "~/projects/shared"   # default <data>/users/<name>/workspace
# budget_daily_usd = 2.0       # unset = the profile's [budgets] apply
# commands = ["status", "help", "skills", "skill", "history", "memory"]
# escalate = ["bash"]               # always the admin's call (D9)

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
ka-agent identity list                               # prints user + role
ka-agent user invite --role guest --group dev        # account + binding
ka-agent user list | disable | enable | reset | export | purge
```

A role not defined in the active profile's config fails the command naming
the config file. An identity whose role is later removed fails closed
(read-only, engine-muted). Identity changes are audited with the new role
and `identity.role` / `identity.link` actions.

`users.role` is what the runtime enforces once an identity is bound to a
user; `identity link --role` writes both the identity's role column and the
binding to the user that role maps to (creating it on a fresh install).
`ka-agent user invite` is the account path: it mints a code, the user
redeems it at `/invite` with a password, and the user is unbound to any
channel until an identity is linked.

## Users, groups and partitions

| Layer | Store | Partition |
| --- | --- | --- |
| user | `users`, `users_groups`, `groups` | `<data>/users/<username>/{memory,notes,attachments,engine-*,ka,workspace}` |
| session | `sessions.username` | one conversation per user, adopted at first message |
| identity | `identities.username` | any number of channel identities per user |
| audit | `audit.username` (beside the hash chain) | per-user views |
| web | `web_sessions`, `invites` | one login per user, role-derived reach |

An operator (role `admin`) keeps the profile's own directories; everyone
else lives under `<data>/users/<name>/`. An existing install's
`<data>/roles/<r>` trees are moved there once on first start.

Effective model policy = union of the user's groups' `models_allow` (absent
or empty ⇒ every catalog model), default = the first non-NULL
`models_default` across the user's groups in alphabetical order, else the
profile's model. A model outside the policy is a named refusal, never a
silent fallback. A user's `quota_daily_usd`, when set, overrides the
role/profile daily budget.

Effective knowledge = KBs whose `kb_access` intersects the user's groups,
plus KBs the user owns, plus (role `admin`) every KB. An unauthorized KB
answers `no knowledge base matches`, never "forbidden" (no existence leak).

## Workplace channels (Slack, Discord)

One app install serves the whole workspace, so the daemon is the single
process answering it:

```toml
[slack]
enabled = true
app_token_secret = "slack.app"      # xapp-…, connections:write
bot_token_secret = "slack.bot"      # xoxb-…
teams = ["T0123ABC"]                # allowed workspaces
auto_provision = false              # create a user for an unknown sender

[discord]
enabled = true
bot_token_secret = "discord.bot"
guilds = ["1234567890"]
auto_provision = false

[channels]
shared_channel_role = "channel"     # built-in: readonly, no grants
default_role = "member"             # role an auto-provisioned user gets
```

- A DM is the person's own private session (`slack:<team>:<user>`,
  `discord:<guild>:<user>` / `discord:dm:<user>`).
- A shared channel answers only on a mention, and the turn runs as a
  **per-channel account** (`slack-<team>-<channel>`,
  `discord-<guild>-<channel>`) holding `channels.shared_channel_role`: its
  own partition, no personal memory, notes or grants. The default
  `channel` role is read-only and grants nothing.
- Workspace/guild allowlists gate everything; an unknown workspace is
  ignored with one `<platform>.unauthorized_workspace` audit row.
- Approval cards render as native buttons; `Runtime::decide` authorizes
  server-side (the acting user or an admin), so a click by anyone else is
  refused.
- Share-time provenance: before posting to a shared channel, the answer's
  `turn_sources` groups are intersected with the channel account's groups;
  a miss blocks the post with an admin notice and a `provenance.blocked`
  audit row.

## Enforcement points

| Layer | Mechanism | Failure mode |
| --- | --- | --- |
| Spawn | `[features] disable` = `[tools]` + role `disable`, merged under the operator layer, cross-checked with the engine parser | worker refuses to spawn, spec named |
| Turn | `Submit.allowed_tools` = role `allow` (or cron `tools` ∩ role allow) | tool filtered from the model specs and rejected at admit |
| Mode | role `mode` ceiling; worker re-asserts it after strand replay | a looser `/mode` clamps to the ceiling |
| Commands | role `commands` + `help`/`status` floor; `update`, `keys`, `quit`, `persona`, `config` are admin-only whatever the list says; `model` follows the group allowlist | one-line refusal |
| Files | `__guard`: every other user root, every role root and the profile tree are write-protected; user reads of the profile's memory/notes/strands/store are refused | tool call denied |
| Memory | disjoint user roots; accepted candidates land in the proposing user's root | n/a (routing) |
| Notes | `turn.username` store key → `<data>/users/<u>/notes` | absent key = admin (in-flight turn) |
| History | FTS/`/history` scoped to the user's sessions | other users' turns are invisible |
| Approvals | self-scope cards go to the acting user's surfaces and their decision; shared-resource, over-quota and `escalate` hits go to the admin queue; a foreign decision is refused and audited (D9) | `approvals are decided by the admin role` |
| Re-auth | every approved intent re-resolves the actor at execute time: disabled user, removed role or withdrawn grant ⇒ deny + `approval.stale` | the external write never happens |
| Models | the user's groups' `models_allow` union; a non-allowed model is refused by name | named refusal |
| KBs | KBs whose `kb_access` intersects the user's groups, plus owned, plus all for admin; `kagent__kb_search`, `@kb:` refs and the web KB route share the check | `no knowledge base matches` |
| Budget | the user's `quota_daily_usd` when set, else the role cap, else the profile; `meta["budget:user:<username>"]` | that user pauses; nobody else |
| Audit | every row carries the acting username beside the chain, and the role | n/a (stamped) |
| Cron | creator username in the payload; tools re-clamped at every fire; `/cron list`, `/reminders` and `/cron rm` are scoped to the caller's user | a user never sees or cancels another's jobs |
| Users | `user disable` clears web sessions, silences identities, pauses crons; identity resolution refuses disabled users | no reply + one `<channel>.unauthorized` audit row |

## Web relay and browser sessions

- `ka-agent user invite [--role member] [--group dev]` prints a one-time
  code (24 h, five attempts, constant-time compare). `GET /invite` redeems
  it with a new password and creates the account (`argon2id`).
- `GET /login` takes username + password; 5 failed verifies per username
  per 15 minutes lock that username out (audited).
- Sessions live in the `web_sessions` table (survive restarts, swept on
  access) behind the same `HttpOnly; SameSite=Strict` cookie.
- A non-admin user's cookie reaches `/` (their chat and history),
  `/chat/send`, `/upload`, `/file/*` and `/events` only. `/events` is
  filtered server-side: a stream carries envelopes stamped with its own
  `web:<user>` source, never another user's turns or admin-only events.
- Admin (the user whose role is `admin`) sees the full panel; the role is
  re-derived per request from the user row, never trusted from the captured
  session.
- Every mutating form POST requires the `X-Ka-Agent: 1` header, so only the
  panel's own JS can post.
- The daemon never trusts a username asserted by the client.

## Delivery

Turn-scoped events carry their source (`telegram:<uid>`, `matrix:@user:domain`,
`console`, …). Telegram keeps one chat per sender and Matrix one room per
sender; escalations (admin approval cards, memory proposals) go to the
admin's chat/room, self-scope cards to the asking user's own surface. A
message for another user never steers into or submits over that user's live
turn: it is queued on their own session.

## What users do not get

- Their own provider keys, bot token or model catalog.
- Approval authority over shared resources, config/keys/persona/model
  commands.
- OS-level isolation from each other: every user runs as the same UID.
  Partitions are filesystem + `__guard` + per-user engine HOME, and the
  bubblewrap-per-engine profile measured in ADR-0003 did not clear its cost
  gate, so it ships disabled (`runtime.sandbox_engine = false`). Revisit
  when a deployment has untrusted members.
