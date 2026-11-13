# Team edition — preliminary research and roadmap

Status: implemented (2026-11-13); the research that scoped the work.
Sharpened the same day by a 3-round interview whose decisions are recorded
in §1.1. Each milestone shipped behind its own ADR (0003 multi-session
runtime, 0004 org/users/approvals) and the spike numbers; the contracts
(`docs/contract/rbac.md`, `web-ui.md`) carry the normative behavior.

Inputs: the external capability review (2026-11-13, with its eight test
cases), first-hand doc reads of [Open WebUI](https://docs.openwebui.com/features/),
[LibreChat access control](https://www.librechat.ai/docs/features/access_control),
[Cloudflare OS](https://blog.cloudflare.com/cloudflare-os/),
[OpenClaw gateway security](https://docs.openclaw.ai/gateway/security) and
[Hermes features](https://hermes-agent.nousresearch.com/docs/user-guide/features/overview),
plus the repo's own contracts (ADR-0001, ADR-0002, `docs/contract/rbac.md`,
`docs/design/web-relay.md`, `docs/design/v3-features.md`).

## 1. Product frame — the decisions

The external review is right that two framing decisions condition
everything. Both are settled:

1. **One organization, not SaaS.** A *team edition*: one install serves one
   company at small scale (5–15 users). This is OpenClaw's "one trust
   boundary per gateway" with the boundary being the organization
   ([OpenClaw](https://docs.openclaw.ai/gateway/security)), and it matches
   ADR-0001's revisit trigger without jumping to full shared hosting.
   Multi-org SaaS stays a non-goal until a real requirement exists.
2. **Ask-first actions, read-free data.** ka-agent's rule of the house —
   external writes need grants and approvals — carries over unchanged.
   The team edition adds *who* approves and *whose* authority an action
   uses (§1.1 approval routing), not autonomy.

What this is **not**: a rebuild into a platform (Open WebUI scale,
stateless replicas, model playground) or an enterprise suite (SAML, SCIM,
ABAC, data residency). The differentiators stay: single daemon, durable
queue, approvals with expiry and intent dedup, grants, audit chain,
doctor, zero telemetry.

### 1.1 Interview decisions (2026-11-13)

| # | Decision | Choice |
| --- | --- | --- |
| D1 | First users | The operator + one concrete SME; deployment on a small **VPS with its own domain** |
| D2 | Scale (12 months) | 5–15 users |
| D3 | Channel tríada | **Telegram + Slack + Discord**, in that order of arrival (Telegram already exists); Teams only on explicit customer demand |
| D4 | Dominant use cases | Chat + personal tasks, **document Q&A (RAG in early scope)**, automations, assisted dev, shared knowledge, MCP query APIs |
| D5 | Sequencing | **One merged milestone**: ADR-0003+0004 together in P0; users + concurrency built as a unit (single-session core makes even 2 concurrent users queue) |
| D6 | Knowledge model | Knowledge bases with **per-group access**; sources: manual upload (MVP), then Notion/git/Dropbox sync; **no per-document ACL sync** |
| D7 | Model economics | **The org pays** (one shared key set) with per-user/group quotas on top |
| D8 | Web auth | **Local username + password (argon2)**, admin-minted invites, admin resets — no email infrastructure |
| D9 | Approval routing | **Policy per action**: within one's own grants and quotas the user approves their own action; anything touching shared resources or exceeding quota escalates to admin/owner |
| D10 | Offboarding | **Pause everything; the admin decides** what to reassign or delete (auditable, reversible) |
| D11 | Execution isolation | **Bubblewrap per user engine** (extend the plugins sandbox) when concurrency lands; cost measured in the P0 spike |
| D12 | Retention | **Configurable, default keep-everything** (internal tool); RGPD knobs by config — per-user export/delete at offboarding; audit chain immutable |
| D13 | MVP cut line | **Web accounts + concurrent chat + model permissions/quotas + one shared KB with upload**; connectors and Slack after |

D9 is a contract change, not a feature: `docs/contract/rbac.md` today says
approvals are decided by the admin role only. Generalizing that is
ADR-0004 scope, with the escalation boundary (own grants/quotas vs shared)
specified per action class.

## 2. Where ka-agent stands

### Assets already in place

| Need | Today |
| --- | --- |
| Per-subject partitioning | ADR-0002 roles partition session, memory, notes, workspace, engine HOME, grants, toolset, mode ceiling, budget, commands, cron and audit (`docs/contract/rbac.md`) |
| Approval machinery | Intent log deduplicated by content hash, expiry-deny, admin-only decisions, diff cards |
| Budgets | `budgets.daily_usd` / `daily_tokens` per profile and per role (`budget_daily_usd`) |
| Web surface | Chat + admin panel, role-scoped chat-only sessions, role-filtered SSE, CSRF/cookie hygiene (`docs/design/web-relay.md`) |
| Channels | Telegram + Matrix with pairing, identity linking, attachments, voice-in |
| Governance primitives | Grants (allow/ask/deny paths), MCP bridge with `secret:` refs, audit chain with role stamps, signed plugins in bubblewrap |
| Multi-instance | Profiles = independent data roots, side-by-side services |

### Structural blockers for team use

| # | Blocker | Evidence |
| --- | --- | --- |
| B1 | **Single-session core.** One engine at a time; a message from another role queues until idle; role switch = kill + respawn worker. N users cannot chat concurrently. | ADR-0002 consequences |
| B2 | **No users.** `identities` bind a channel ID to a *role*, not to a person; no groups, no lifecycle (invite/disable/offboard), web access is pairing codes minted per role. | `docs/contract/rbac.md` |
| B3 | **No resource model.** Persona, skills, model, knowledge are operator-global per profile; `/model` is admin-only; nothing shareable exists with per-resource ACLs. | README, `rbac.md` |
| B4 | **Channel adapters are personal-IM shaped.** One bot token polled by one daemon, DM-first, one chat per sender. Workplace channels are *workspace-app* shaped: one app install serves every member; senders arrive as workspace user IDs; threads and interactive components are the native UI. | README channels, `v3-features.md` #9 |
| B5 | **App-install economics force a shared daemon.** In Slack/Discord/Teams one app is installed once per workspace/tenant — the bot identity and its credentials belong to the *org app*, not to N per-user bots. Instance-per-user (ADR-0001) therefore cannot serve a workplace channel without N separate apps. This fires ADR-0001's revisit trigger for the team track. | ADR-0001 |

B1+B5 are the architecture decision; B2+B3 are the product decision the
user actually sees (accounts, profiles, model permissions).

### What the field teaches (and what not to copy)

| Reference | Adopt | Don't copy / defer |
| --- | --- | --- |
| **Open WebUI** | Models as config wrappers (base model + prompt + tools + knowledge) with per-model access control; admin analytics (tokens, cost); feature switches per role | Full platform shape: stateless horizontal scaling, 13 vector DBs, arena/evals, SCIM |
| **LibreChat** | Principals = user / group / role / everyone; per-role feature switches (use / create / share per resource type); resource sharing with Viewer/Editor/Owner and "always ≥1 owner"; delegated admin capabilities instead of one god-role; its honesty that Viewers can coax attached content out via chat | Entra ID group sync, Admin Panel as separate app (ours is one panel), SSO for now |
| **Cloudflare OS** | Agents start with no access and request per-resource grants; credential holders that mediate + audit every access (Gatekeepers ≈ our grants/MCP bridge grown up); the **observation log** — provenance of what an agent saw, ACL re-checked when the output is shared; share-the-definition vs share-the-instance (blueprint has no data/credentials) | Dynamic Workers/apps platform, Cap'n Web RPC, AI Gateway as product |
| **OpenClaw** | Conservative defaults + one command that audits drift (≈ doctor); requester-scoped controls; DM session isolation per user; context-visibility vs trigger-authorization split in groups; untrusted-content wrapping; explicit rate limits | Treating one gateway as a barrier between adversarial users — their docs say split trust boundaries instead; we adopt the org-as-boundary frame |
| **Hermes** | Credential pools (multi-key rotation on rate limits); provider routing/fallback governance; cron jobs that deliver results to any platform; curated, card-gated memory (already our stance) | Voice/wake-word/browser breadth; batch trajectory generation |

Teams-specific finding: a Teams bot requires an Azure Bot Framework
registration and a **publicly reachable HTTPS messaging endpoint**
([Microsoft Q&A](https://learn.microsoft.com/en-us/answers/questions/5706215/teams-bot-not-receiving-messages-when-using-a-self),
[Bot Service FAQ](https://learn.microsoft.com/en-us/azure/bot-service/bot-service-resources-faq-security?view=azure-bot-service-4.0)).
Slack (Socket Mode) and Discord (gateway) can run over outbound WebSocket
with no public inbound endpoint — matching our loopback-first stance. This
confirms D3's order and keeps Teams optional.

## 3. Roadmap

Sizes follow `v3-features.md` bands (S < 1 day, M days, L ≥ week). Every
milestone ships green against the existing contract suite and ends with
its e2e scenarios (§5).

### P0 — ADRs and spikes (M)

Per D5, both ADRs land together and feed one merged milestone:

- **ADR-0003 multi-session runtime** (supersedes ADR-0002's single-session
  core): how the daemon runs M concurrent engines — worker-per-session
  pool with LRU eviction and idle timeouts (the `worker_idle_timeout_secs`
  machinery), resource ceilings, crash isolation per worker. Spike
  questions: real memory/CPU per ka worker, startup cost, how the port
  layer and `sessions` schema change when the key becomes the user's
  session, and — per D11 — the measured cost of running each engine in a
  **bubblewrap** sandbox (extend the plugins sandbox; if the cost is
  prohibitive, document the same-UID boundary instead and revisit).
- **ADR-0004 organization & user model**: `users` and `groups` tables;
  `identities` gain `user` (a channel identity maps to a person; the role
  becomes a permission *set* the user holds, not the partition key);
  partition key generalizes from role to user, reusing `roles.rs`
  partition machinery. Also specifies D9 (approval routing semantics and
  the escalation boundary), D10 (offboarding = pause + admin decides) and
  D12 (retention knobs, default indefinite).

Exit: both ADRs accepted; spike report with measured numbers; no product
change yet.

### M1 — Users, concurrency, models, knowledge seed (XL, the merged milestone)

Implements ADR-0003 + ADR-0004 as one unit. The MVP cut line (D13) is the
milestone's first shippable slice: **web accounts + concurrent chat +
model permissions/quotas + one shared KB with upload**. Everything else in
this section follows inside the same milestone.

- **User lifecycle (D8, D10)**: invite codes (evolution of `pair web`),
  argon2 local accounts, per-user sessions; disable/offboard = revoke web
  sessions, unlink identities, **pause their crons and automations for the
  admin to reassign or delete**; per-user export/delete for RGPD (D12).
- **Groups + permission sets**: LibreChat-style feature switches per role
  (use agents / create / share; web chat; channels; cron; memory; KB
  access) layered on `[[role]]`, which already clamps commands, tools and
  mode.
- **Model permissions (D7)**: the catalog (models.dev merge) becomes a
  governed surface — per-user/per-group allowed models, default model per
  user, per-model budget tiers; `/model` stops being admin-only and
  becomes permission-filtered per caller. One org key set + per-user
  quotas.
- **Multi-session runtime**: concurrent workers (bubblewrap per engine if
  the spike approves), per-user session and queue, intake fairness,
  concurrency caps, event fan-out generalized from role-filtered to
  user-filtered envelopes.
- **Approval routing (D9)**: a user approves actions **within their own
  grants and quotas**; actions on shared resources or over quota escalate
  to the admin/resource owner. Re-authorization at execute time — a
  revoked grant voids a queued approval (§5 case 2).
- **Durable execution**: resume-after-crash semantics for turns; the
  intent content-hash dedup becomes durable idempotency for external
  writes — a resumed run detects the already-executed action instead of
  repeating it (§5 case 3). LangGraph's caution (steps may replay) is the
  spec for which steps must be idempotent
  ([LangGraph](https://docs.langchain.com/oss/python/durable-execution)).
- **Knowledge seed (D6)**: KBs as first-class resources with per-group
  access, manual upload via the panel, retrieval wired into turns for
  permitted users. No per-document ACLs, no source sync yet.
- **Audit [!!]**: audit rows gain `user` beside `role`; admin views per
  user.
- **Retention (D12)**: configurable windows per data class, default
  keep-everything; audit chain stays immutable and append-only.

Exit: a 3–5 person pilot (operator + SME) uses the panel concurrently with
their own sessions, models, budgets and one shared KB; offboarding e2e
passes; kill -9 mid-turn resumes without duplicate external effects;
approval-after-revocation is denied; no cross-user leakage in
`rbac_e2e`-style tests keyed by user.

### M2 — Channel tríada: Slack + Discord beside Telegram (L each)

One adapter family beside the personal-IM one, workspace-app shaped
(B4/B5). Telegram stays first-class (already personal-IM shaped, already
linked to identities). Shared contract first, then per-platform:

- **Shared connector contract**: identity mapping (workspace user → ka
  user, auto-provision gated by workspace allowlist), context model (DM =
  private per-user session; shared channel = shared context with mention
  gate and explicit disclosure rules — OpenClaw's context-visibility vs
  trigger-authorization split), approval cards as native interactive
  components (D9 routing renders as: approve button for the acting user
  where in-policy, escalation card for admin/owner), threading, file
  exchange, rate limits, delivery/egress rules, revocation (app removed /
  member leaves).
- **Slack first**: Socket Mode (outbound WebSocket, no public endpoint on
  the VPS beyond the existing web reverse proxy), workspace install, DMs +
  mention in allowed channels, Block Kit approval buttons. Spike: OAuth
  scopes, event allowlists, interactivity response URLs, per-method rate
  tiers, file scopes.
- **Discord second**: gateway WebSocket, guild allowlist, threads;
  `MESSAGE_CONTENT` is a privileged intent — spike intent approval flow,
  interactions, role/member mapping.
- **Teams on demand only** (D3): Azure Bot registration + public HTTPS
  endpoint makes it the heaviest and the only one requiring inbound
  exposure; also the door to Graph/on-behalf-of connectors later.

v3-features' "Rejected: Discord/Slack channels" and "Rejected: group
chats" entries are superseded for the team track by this milestone — they
were rejections under the personal-assistant frame (ADR-0001), which §1
explicitly revisits.

Exit per platform: workspace e2e against a protocol fake — two users, one
shared channel, one approval round-trip on the platform's own UI,
unlinked workspace refused.

### M3 — Shared resources, connectors, governance (L)

- **Knowledge connectors (D6)**: Notion, git, Dropbox sync into the
  existing KBs (org credentials in the secret store, per-KB group gating,
  deletions propagate; propagation delay documented honestly — Onyx's
  lesson). Per-document permission sync from sources stays a non-goal.
- **Agents as shareable resources**: persona + skills + allowed models +
  knowledge bundle with LibreChat's access roles (Viewer/Editor/Owner,
  ≥1 owner invariant) and its warning that viewers can extract attached
  content — share definitions, keep run state per user (Cloudflare OS's
  blueprint-vs-app distinction).
- **Provenance / observation log [!!]**: record which documents, KBs and
  tools fed a turn or artifact; re-check ACLs when a derived result is
  shared (§5 case 5) — the one Cloudflare OS idea worth adopting early
  even in primitive form (a per-turn source list checked at share time).
- **Usage & cost dashboards [!!]**: per-user/per-group/per-model spend
  from existing `turns.model` accounting; budget states surfaced.
- **MCP governance for the query APIs (D4)**: approved-server registry,
  per-user tool gating on top of the existing bridge/`secret:` refs.
- **Connector credentials**: org-owned bot credentials in the secret
  store (as today); per-user delegated credentials (on-behalf-of) only
  when a connector needs user-scoped access — design with the first real
  connector, LibreChat's SharePoint flow as reference.

Exit: a shared team agent with per-user runs and owner-routed approvals;
sharing a summary built from restricted KBs is a decision, not a leak.

### Deferred — with revisit triggers

| Deferred | Why | Revisit when |
| --- | --- | --- |
| SSO (OIDC), SAML, SCIM, MFA-idp | Local accounts + invite codes cover ≤15 users (D8); IdP sync is real enterprise weight | An org mandates its IdP to adopt |
| ABAC / attribute rules | RBAC + groups + resource ACLs suffice at this scale | Compliance or multi-department needs |
| Multi-org SaaS tenancy | §1 decision | A hosted product requirement |
| Per-document permission-synced RAG (Onyx/Glean class) | D6 chose per-KB group gating; doc-level sync is the most expensive subsystem to rush | Multi-department orgs with source-held ACLs |
| Evals infrastructure (Langfuse class) | Useful once there are shared agents to regression-test | After M3 stabilizes personas/skills |
| Model gateway / routing service (LiteLLM class) | Catalog + fallback + per-user allowlists already planned | Multiple providers with per-team routing pain |
| Per-engine OS isolation beyond bubblewrap (OS users) | D11 chose bubblewrap; measure first | Untrusted-member deployments or regulated orgs |
| Horizontal scaling / stateless replicas | Single daemon is the product's shape | A real second-server need |

## 4. The [!!] items — where each lands

| Marked concern | Milestone | Concrete shape |
| --- | --- | --- |
| Permisos más allá de RBAC | M1, M3 | Feature switches, per-user/per-group model ACLs + KB gating, resource Viewer/Editor/Owner, approval routing (D9) |
| Aislamiento | M1 | Partitions keyed by user (`__guard`/FTS/memory), bubblewrap per engine if the P0 spike approves (D11), honest same-UID doc otherwise |
| Fiabilidad (durable execution) | M1 | Concurrent sessions, resume, re-auth at execute, durable idempotency |
| Auditoría y calidad | M1 (user-stamped audit), M3 (views, provenance); evals deferred | Audit chain already exists; add user column + admin views |
| Costes y capacidad | M1, M3 | Per-user quotas, org cap, model tiers (D7), dashboards |

## 5. Acceptance scenarios (adapted from the external review)

Mapped to milestones; each becomes an e2e in the repo's style
(`rbac_e2e`, `isolation_e2e`):

1. A user asks about another department's folder/KB → unauthorized docs
   never reach context, citations or reply. **(M1: partitions + KB
   gating; M3: provenance)**
2. Access revoked while a task awaits approval → re-authorization at
   execute; stale approvals lose withdrawn privileges. **(M1)**
3. Agent files a ticket and the process dies before recording → resume
   detects the prior action, no duplicate. **(M1)**
4. External document contains secret-extraction instructions → content
   is untrusted data; tools lack unlimited authority (grants + ask mode
   already the floor; add wrapping rules for fetched content). **(M1–M2)**
5. A user shares a summary built from restricted docs → explicit
   decision on who sees the derived result. **(M3)**
6. Two conversations share a browser/sandbox → defined sharing, checked
   acceptable. **(M1)** (Per-user engines default avoids the OpenHands
   shared-sandbox class entirely.)
7. Employee with active automations leaves → credentials revoked, jobs
   paused for admin reassignment (D10). **(M1)**
8. A run enters a tool loop → stopped by time/step/spend limits with
   recoverable state. **(M1; budgets/iteration caps partially exist)**

## 6. Next detailed-research agenda

Per-implementation deep dives this roadmap hands off to (the user's
"investigar en mucho más detalle" list), in order:

1. **P0 spike** — worker memory/startup measurements, bubblewrap per
   engine cost, sessions schema migration, envelope fan-out, port-layer
   changes.
2. **Users/groups/permissions model** — schema, invite/offboard flows
   (D10), feature-switch matrix, approval-routing boundary (D9), web auth
   (argon2, session rotation, rate limits) extending web-relay's
   checklist.
3. **Model governance** — catalog ACLs, per-user defaults, budget tiers,
   `/model` permission filtering (D7).
4. **Knowledge base core** — KB storage/retrieval shape on the existing
   store, per-group gating, upload flow; then Notion/git/Dropbox sync
   connectors.
5. **Slack connector** — app manifest, scopes, Socket Mode limits,
   events/interactivity, identity mapping, rate tiers, files.
6. **Discord connector** — intents (privileged `MESSAGE_CONTENT`),
   interactions, guild allowlist, threads.
7. **Shared-agent resource model** — ACL storage, share UX, provenance
   log shape, owner-routed approvals.

## 7. Relations to existing docs

- Supersedes for the team track: `v3-features.md` "Rejected: Discord/
  Slack channels" and "Rejected: group chats" (rejected under the
  personal frame; the personal track keeps its own rejections).
- Fires ADR-0001's revisit trigger (B5) — resolved by ADR-0004's
  single-org shared model, not by multi-tenant SaaS.
- Extends ADR-0002: role partitions become user partitions; its
  non-goals (per-role models, per-user web accounts, OS isolation) are
  precisely M1–M3's scope list. D9 amends `rbac.md`'s "approvals are
  decided by the admin role" via ADR-0004.
- Extends `web-relay.md`'s v1 choice ("don't rebuild a user-account
  system in v1") — v1 was right; the team edition is the v2 that adds
  accounts (D8). Its optional OpenAI-compatible `/v1` shim stays an
  integration surface, never the product front end: approvals, cards and
  per-user sessions don't fit chat-completions semantics.
