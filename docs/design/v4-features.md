# v4 feature catalog — 2026-11-18

Baseline: v0.14.5, `main` @ `2dcb699`, engine pin `247e99e`. Companion to
`v3-features.md` (same bands: priority = impact × frequency, P0 ≥ 6, P1 3–5, P2 ≤ 2;
effort S < 1 day, M = days, L ≥ week; risk 2 = approvals/security/engine pin/protocol).
This document is a catalog only — nothing here is implemented.

## Method

Three vygr research runs (14-harness feature inventory; always-on assistant daemons;
failure-mode disconfirmation) plus four doc scouts reading official docs of Claude Code,
Codex CLI, Gemini CLI, aider, opencode, Goose, Amp, Cursor, Warp, Plandex, OpenHands,
Devin and Jules; then two independent evaluators (product-value and architecture/risk)
scored all 42 candidate ideas; the intersection plus designated cluster first-slices
became eight planner commissions, each cross-reviewed by a different reviewer against
the tree at `2dcb699`. Evidence trail: `agents/voyager/20261118-21*`, evaluator tables
and per-feature plans in the session's `local://` artifacts (`eval-value.md`,
`eval-arch.md`, `plan-*.md`, `review-*.md`). Boundary rule unchanged from v3:
everything above `KaPort` is ka-agent work; below-port essence goes to the Handoff
section, never an Adaptation.

## Doc drift found during verification (fix regardless of this catalog)

- `README.md` "The ka pin" cites `193ad5d`; every `ka-*` dependency is pinned at
  `247e99e` at `2dcb699`.
- `docs/contract/ka-port.md` §4 (gap G4: "Ka silently drops a prompt that arrives while
  a cycle is running") is stale: steer ships end-to-end (`busy_messages = "steer"` default,
  `PortSession::steer` → `WorkerCommand::Steer`, e2e
  `message_during_a_turn_steers_the_running_reply`); §1's command table lacks the
  `Steer` row (worker-protocol.md likewise).
- `refs.rs:210` `turn_ref` resolves `@turn:` across user partitions with no check —
  real cross-user read gap, fixed by the B17 feature below regardless of its priority.

## Top 18

| # | Title | Priority | Effort | Impact |
|---|-------|----------|--------|--------|
| 1 | B7 Approve→rule + tested command rules | P0 | M | every approval can become a never-again-asked rule; the top daily-friction killer |
| 2 | Shadow-git checkpoints + code-only `/rewind` | P0 | M | the safety net under autonomy; OpenClaw's top churn regression was removing it |
| 3 | `@turn:`/`@session:` partition checks | P0 | S | fixes a live cross-user read defect (rbac.md History row) — lands first |
| 4 | Memory candidates as bullet diffs + dry-run | P0 | M | the operator sees the exact delta and budget consequence before accepting |
| 5 | Automation turns excluded from personal memory | P0 | M | closes the cron→memory feedback loop; scheduled turns see machine facts only |
| 6 | Skill `triggers:`/`paths:` + `/skill-doctor` | P0 | M | skills fire when relevant instead of only on explicit `/skill`; their context cost becomes visible |
| 7 | Queue manager | P0 | M | the durable queue becomes visible, editable, reorderable, jumpable on every surface |
| 8 | Interrupt keywords | P0 | S | 'stop' on the phone aborts and re-queues without `/stop` |
| 9 | Trigger model + webhook ingress + preflight | P0 | L (staged) | events from outside become first-class: ingress, cheap preflight gate, per-trigger identity, lineage — gated behind `/watch` and an ADR |
| 10 | Mode-scoped 'always' | P1 | S | durable always-allow owned by Ka grants with mode ceilings; closes gap G12 |
| 11 | Quota/warm-up-aware proposal timing | P1 | S | memory extraction stops spending when the budget is nearly gone |
| 12 | Plan artifact + verify/repair cycle | P1 | M | plan → approve → execute → verified done, with bounded repair attempts |
| 13 | Read-only scout child | P1 | M | delegated research with a strict-subset toolset and escaped, non-authoritative returns |
| 14 | Diagnostics bundle | P1 | M | one JSON for support: doctor + config + recent turns + daemon-side prompt layers |
| 15 | Agent commit attribution | P1 | S | machine-written history is identifiable at a glance |
| 16 | Cron snapshot-at-creation | P1 | S | scheduled jobs stop silently changing meaning on daemon upgrades |
| 17 | Small affordances batch: A6 rate-limit status, A8 presets, A9 Ctrl-L fix, A13 `/archive`, A16 config JSON Schema | P2 | — | five small quality fixes (S–M) |
| 18 | `/history` filters model:/after:/status: | P2 | S | power-user slice riding the FTS machinery |

## Features

### 1 B7 Approve→rule: tested command rules on today's grants
- Research basis: Codex rules — `prefix_rule` carries inline `match`/`not_match` cases, and the harness proposes a rule when the user approves an escalation (https://developers.openai.com/codex/agent-configuration/rules); `execpolicy check` dry-runs a rule list against a command without granting. Ka context: approval fatigue is the ask-mode assistant's top friction (eval-value B7 3×3); today's only remedies are the in-memory `session_allow` (`decide_for_session`, runtime/approvals.rs:197) and hand-edited `[grants]` — nothing durable is ever proposed.
- Adaptation:
  - New `crates/ka-agent-core/src/rules.rs`: `Rule { id, subject, shape, mode, from_approval, created_ms }` with `Shape::AskKey { key }` (the exact `session_allow_key` identity, runtime/approvals.rs:9, made durable), `Shape::Tool { name }`, `Shape::Command { prefix }` (bash prefix, e.g. `cargo test`). Matcher `Rule::matches(question, detail, last_call, mode_rank)` — mode-gated from day one: `mode_rank` (allow=0 < accept_edits=1 < ask=2 < readonly=3) moves from `set_mode`'s local fn (runtime/config.rs:564-575) into `ka_agent_core::config`; a rule fires iff `mode_rank(current) <= mode_rank(rule.mode)` (current + more permissive only — B8's semantics, landed here so no ungated window exists). `Command` rules match only *provably plain* commands — no `;` `&&` `||` `|` `$(` backtick `<`/`>` — reusing the guard's tokenizer (`split_program`/`unquote`, apps/ka-agent/src/guard.rs:177-254, moved into core).
  - Honest shape ladder (R2): plain bash ask → `Command(prefix)`; **compound bash ask → `AskKey` only** — auto-allows solely the byte-identical (question, detail) pair, never a bash-wide grant; other tool asks (`Live.last_call`, runtime/mod.rs:132) → `Tool(name)`; nothing derivable → `AskKey`. A `Tool` rule can never match a bash ask (pinned by its test cases) — there is no path from one compound approval to "always allow bash".
  - *Tested* rules — `cases_for()` defined for every shipped shape (R3): `Command` — match = the observed command, not_match = `pfx && …`, `pfx; …`, `pfx | sh`, `$(…)` splices; `Tool` — match = the exact hand name, not_match = any differing name and every bash ask; `AskKey` — match/not_match = the exact pair vs any mutated question or detail. A rule failing any case never lands (fail-closed, neg-evid §3). `ka-agent rule check '<command>'` dry-run (in apps/ka-agent/src/grant.rs's action family, beside `GrantAction`, main.rs:47) prints which rule fires and why, granting nothing.
  - Store: the **next schema migration, number assigned in landing order, second lander renumbers** (forward-only, ka-agent-store/src/lib.rs:98) adds `grant_rules(id, subject, kind, pattern, mode, from_approval, created_ms)` + repository methods beside store/approvals.rs. Loaded fresh at every consultation — never cached, the MCP-bridge rule (grants.md §Enforcement).
  - Enforcement ordering (R1 — NOT beside `session_allow`): in the `PortEvent::Ask` arm the rule check runs **after** D9 `policy::route` (runtime/events.rs:466-493) and after the `awaiting` transition (:503-518), and only when `awaiting && escalate_reason.is_none()` — a live turn and an ask the acting user themselves decides (subject = `who.username`). Escalate-list, shared-resource and over-quota asks route to the admin queue untouched; orphan/dead asks keep the deny (:539-543) — a rule never answers them. A hit calls **`Runtime::answer_allow`** — the single Allow-settle helper of the checkpoints plan's B1 v2 (runtime/approvals.rs: checkpoint → `port.answer(Allow, choice 0)` → audit with the caller label; whichever of B1/B7 lands first extracts it from `decide`'s tail :166-194, the other reuses it) — with audit label `Allow (rule <id>)`, then the same settle tail as `decide` (:171-194: store settle, `ApprovalSettled`, `AwaitingApproval→Running`). All three Allow paths route through it — `decide`'s Allow arm, the session_allow auto-answer (inline at events.rs:439-441 today), and this rule arm — so rule-allowed writes take the checkpoint too and `/y`, session and rule settles are indistinguishable downstream. No card is emitted on a hit.
  - Approve→proposed-rule: `/y <id> always` — a third decision form in channels/commands/approvals.rs:39-92 mirroring the `session` form — computes the shape per the ladder above, shows the exact rule text in its reply, validates the cases, writes the row with `from_approval`, answers `Allow` (choice 0 — engine choice 1 stays never-sent). Offered only on non-escalated self-scope cards (escalated refuse: D9). Approval-fatigue trigger: a repeat counter beside `session_allow` (runtime/mod.rs:162) makes the Nth identical card carry `asked N× — /y <id> always makes it a rule`.
  - Surfaces: TUI bare `a` on the armed card + `/y <id> always` (apps/ka-agent/src/chat.rs:508-528; footer :348); web Always button beside Allow; `/rules [rm <id>]` lists the caller's rules (admin: all + rm); doctor's grants line (apps/ka-agent/src/doctor.rs:177) appends `, N rules`. Audit: `approval.always`, `rule.add`, `rule.rm`.
  - Contracts: grants.md gains a "Durable rules" section (shapes, per-shape cases, ordering after D9, one settle site); rbac.md Enforcement "Approvals" row gains the non-escalated clause.
- Acceptance (grants_e2e.rs style — spawn + scenario + Tap):
  - `always_command_rule_stops_second_ask`: ask mode, two turns running `cargo test --color never`; first raises the card, `/y <id> always` replies with the rule text (`cargo test`); second turn: zero new approval events, the `✓ rule` Note, turn finishes.
  - `rule_never_matches_compound_commands`: `cargo test && rm -rf x` still raises a card; `rule check` prints no match naming the non-plain operator; `/y <id> always` on it lands an `AskKey` rule — asserting `Tool{"bash"}` is unreachable.
  - `escalated_and_over_quota_asks_never_rule_allowed`: role with `escalate = ["bash"]` → card carries `[escalated: …]`, `/y <id> always` refused, and a budget-exceeded ask escalates with zero `Allow (rule` audit rows.
  - `tool_rule_scoped_to_hand_name`: a bridge-tool ask lands a `Tool` rule; a different tool's next ask still raises a card; after `/new` the identical tool ask auto-allows with the rule note.
  - `ask_key_rule_survives_new_session`: the compound-command `AskKey` row auto-allows only the byte-identical ask after `/new`; a mutated variant raises a card.
  - Unit: per-shape `cases_for` sets; a failing-case shape writes no row; the matcher refuses `Tool`→bash; `mode_rank` gating (ask-born rule silent in readonly, firing in accept_edits).
- Priority: P0 · effort M · risk 2 (touches approvals/security; mitigated: rules only downgrade self-scope live asks, D9 ordering intact, test-gated creation, one settle site).

### 2 Shadow-git checkpoints + code-only `/rewind`

- Research basis: Gemini checkpointing — commit the workspace to a shadow repo before each write-bearing tool, `/rewind` restores and re-proposes the original call (https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/checkpointing.md); Claude checkpointing with code/conversation restore kept independent (https://code.claude.com/docs/en/checkpointing); digest §5 negative evidence: removing checkpoint restore with no replacement was OpenClaw's top churn regression — this is the safety net under autonomy. Slice shape per the versioning-cluster sequencing (eval-arch C3): checkpoint before each *approved* write + code-only `/rewind` — no engine change, no staging (B2 stays engine-side).
- Adaptation:
  - **One Allow-settle site.** New `Runtime::answer_allow` in `runtime/approvals.rs` beside `decide`: (1) create the checkpoint, (2) `port.answer(…, Allow)`, (3) audit `approval.decide` with a caller label (`operator` / `Allow (session)` / B7's `Allow (rule <id>)`). Every Allow path routes through it and through nothing else: `decide` (`approvals.rs:170`, for `Allow|Always`; Deny keeps the direct call), the Ask-arm session-allow auto-answer (today inline at `runtime/events.rs:439-441`), and the B7 durable-rule arm in the same Ask arm (feature 1 names this same helper — the wording is coordinated). At each call the engine is blocked on the ask, so by ka-port.md §4 the workspace is quiescent and the snapshot cannot race the write it precedes. Denied/expired/stale asks never reach `answer_allow`. A rule-allowed write must never skip the net — that is the point of the single site.
  - **Shadow repo — never the workspace's own `.git`.** New `runtime/checkpoint.rs`: one-time `git init` into `user_checkpoints(<u>)/<short session id>` + `core.worktree` = the session workspace, every op via `--git-dir/--work-tree` (bounded-invocation shape from the memory crate's git backing, `crates/ka-agent-memory/src/lib.rs:150-155, 303-313`; fixed identity constants like `GIT_NAME`); then `add -A` + `commit` per checkpoint. Path: new `Paths::user_checkpoints(username)` = `user_root(username).join("checkpoints")` beside `user_workspace` (`crates/ka-agent-runtime/src/paths.rs:144-146`; operator keeps the profile root, `:98-106`), created in `ensure_user_dirs`'s list (`:154-163`) — a daemon-owned slot, deliberately NOT `user_ka_data` (`:134` is the engine's `KA_DATA_DIR`). Workspace from `store.get_session(&row.session)` (`store/sessions.rs:80` → `Session.workspace`, `core/session.rs:66`). Worktree `.gitignore`s apply (`target/`-class trees stay out); an un-git-ed workspace checkpoints fine.
  - **Index.** The next schema migration, number assigned in landing order (baseline ends at v12, `store/lib.rs:101-113`; feature 1's rules table may take the next number first — the second lander renumbers): `checkpoints(id, session, turn, username, approval, question, sha, created_ms)` + index `(session, created_ms)`; accessor module `store/checkpoints.rs` beside `approvals.rs`. The `ApprovalKind::KaAsk` question/detail (`core/approval.rs:14-22`) is stored for the re-propose reply; rule/session settles carry `approval = NULL`.
  - **`/rewind` command.** One `CommandSpec` in the shared table (`channels/commands/mod.rs:73` — help/menu follow it), handler in `commands/session.rs` (`run_status` pattern, `:8`), `Reach::Everywhere`. No args → newest-first list `N. HH:MM · before <question> (turn <id>)`; `/rewind <n>` refuses unless the session has no open cycle (`SessionStatus` ∉ {Running, AwaitingApproval, Stopping}, `core/session.rs:13-25`), then restores with `checkout -f <sha> -- .` into the worktree — **no `git clean`**: files created after the checkpoint are left alone — audits `checkpoint.rewind` (`core/audit.rs:71` action pattern), replies with the stored question + the checkpointed turn's `inbound_text` (`core/turn.rs:66`) so the operator can re-propose. **A1 (one line):** the TUI additionally seeds its composer with that `inbound_text` via `Editor::set` (`apps/ka-agent/src/term/editor.rs:33`) — opencode `/undo`'s prompt-back-in-composer surface; the conversation half of A1 is H2 below.
  - **Config + pruning.** `[checkpoint] enabled` (bool, default true) and `checkpoint.keep_days` (int 0–3650, default 30) in `config_keys.rs` beside `retention.*` (`:265-272`); pruning rides the existing retention pass (`PruneReport` gains a field, `store/retention.rs:35`; scheduler call site) deleting rows *and* repos past the window — Gemini's 30-day pruning, made explicit.
  - **Failure policy.** A failed snapshot (disk full, git absent) never vetoes an allowed write: `answer_allow` proceeds to the answer, the gap is audited (`checkpoint.failed`) and shows as a hole in `/rewind`'s list. Approvals stay the security boundary; the checkpoint is the net, not a gate — a fail-closed veto here would hostage explicit operator decisions to telemetry.
- Acceptance (`apps/ka-agent/tests/chat_e2e.rs` style — real binary + scripted speaker, cf. `approval_flow_and_queue` at `:727`): `approved_write_checkpoints_and_rewind_restores` — ask-mode scenario `bash {"command":"echo broken > fix.rs"}` inside the workspace, `/y`, a second approved write changes the file; `/rewind 1` replies with the stored question and `fix.rs` matches the first checkpoint's tree. `denied_writes_leave_no_checkpoint` — same scenario, `/n`: no `checkpoints` row and `/rewind` answers `no checkpoints`. `session_allowed_write_checkpoints_too` — `/y <id> session`, then the identical ask again: the ✓ session-allow answer leaves a checkpoint row. `rewind_refuses_while_cycle_open` — `/rewind` while the ask is pending gets the refusal wording and the workspace is untouched. Unit (`runtime/checkpoint.rs` tests): `failed_snapshot_never_vetoes_the_write` — checkpoint root read-only → `answer_allow` still answers, a `checkpoint.failed` audit row exists, `/rewind` lists the gap; restore semantics — gitignored, post-checkpoint, and nested-repo files survive a restore (gitlink dir restored empty, detected).
- Priority: P0 · effort M · risk 2 (the hook sits in the approvals settle path; no engine change, no new inbound surface)

**Contract delta:** `docs/contract/rbac.md` "Users, groups and partitions" — the per-user tree enumeration (`:74`, `{memory,notes,attachments,engine-*,ka,workspace}`) gains `checkpoints`. No other contract changes.

**Handoff (below KaPort → ka-harness-features):** H2 strand fork/branch — the conversation half of `/rewind` (Claude's 3-way code/conversation/both choice, and re-proposing the tool call *into* the strand). Ka Agent can restore code and re-print the prompt; rewinding the strand itself needs a fork op beside `open`/`submit` (ka-port.md §1-2). Same op unlocks A1's edit-and-fork.

**Limits (Gemini-documented caveats, stated in `/rewind` help):**
- Only ask-gated writes checkpoint: `__guard`-pre-approved grant writes (`apps/ka-agent/src/guard.rs`, grants.md "Operator grants") and `accept_edits`/`allow`-mode edits raise no ask — hence no snapshot. (Rule- and session-allowed writes DO checkpoint — this feature's single settle site.)
- Gitignored trees, symlink targets, and bash-made edits outside snapshotted moments are never captured; restores neither revert nor delete them (checkout-only, no clean).
- A nested git repository in the workspace is recorded as a contentless gitlink: restore yields an empty dir, not the nested repo's contents (first slice documents this, doesn't solve it).
- `keep_days` pruning is destructive: past the window that history is gone.
- A failed checkpoint is a hole in the net (audited, visible), never a blocked approval.

### 3 `@turn:`/`@session:` partition checks at expansion (B17 security half — lands first)

- Research basis: Amp threads — `@T-…` pulls another thread's content
  (https://ampcode.com/docs/threads) — supplies the cross-session reference
  shape. The load-bearing fact is a **live defect Review B confirmed at
  `2dcb699`**: `turn_ref` (`crates/ka-agent-channels/src/refs.rs:210`) takes
  no caller and resolves through the global `find_turn_by_prefix`
  (`store/turns.rs:164` — its own comment still says "Global, not
  per-session: one instance is one operator", an assumption schema v12's
  `sessions.username` partition broke). Any authenticated channel user can
  expand another user's turn id into that user's full prompt plus concatenated
  answer — a direct contradiction of rbac.md's History row ("other users'
  turns are invisible"). Id-gated (random `turn_*` ids), so it fires when ids
  leak via logs/transcripts/support; a real partition violation, and this
  feature lands independently of — and before — the `/history` filters below.
- Adaptation:
  - **`@turn:` check**: thread `who` (already in scope in `expand_word`,
    `refs.rs:99`) into `turn_ref` (`refs.rs:210`) and refuse unless
    `turn_username` (`store/turns.rs:362`) matches the caller or the caller is
    admin — exactly the check `history_show` performs
    (`runtime/memory.rs:259-263`). Mismatch renders `[ref @turn:…: not found]`
    (no existence leak, the `@kb:` wording family).
  - **`@session:<prefix>` / `@s:`**: new arm in `expand_word` calling a
    `session_ref` that resolves through a new `Store::find_session_by_prefix`
    mirroring `find_turn_by_prefix` (prefix/tail match, newest `updated_ms`
    wins) and applies the **same partition check at expansion** (rbac.md
    History row): the session's `username` must equal the caller's or the
    caller is admin — otherwise `[ref @session:…: not found]`. Expansion
    content: a header (short id, created date, turn count) plus the session's
    last 3 exchanges (`> inbound` + answer), each truncated under
    `REF_ITEM_MAX` (`refs.rs:18`), ending with the turn ids so `@turn:`
    follow-ups work.
- Acceptance: `turn_refs_of_another_user_are_not_found` (a non-admin
  referencing a foreign turn gets the `not found` marker and stages no source;
  the same id as admin resolves); `session_ref_pulls_last_exchanges_and_cites_turn_ids`;
  `session_ref_of_another_user_is_not_found` (non-admin → marker, admin →
  content; both refs persist the expansion in `turns.inbound_text`, so the
  refusal is auditable).
- Contract: `docs/contract/cron-and-refs.md` §"@references" — the partition
  rule stated for `@turn:` and `@session:` (rbac.md History row now honored at
  expansion); memory.md unchanged.
- Priority: P0 · effort S · risk 1

### 4 Memory candidates as bullet diffs with dry-run accept (B15a)

- Research basis: Gemini CLI auto-memory — every memory change is drafted as a
  unified-diff `.patch`, "parsed, target-allowlisted, and applied atomically
  only when you approve them from the inbox"; skill patches are dry-run before
  they are even shown (https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/auto-memory.md).
  Ka already gates every write through cards; what is missing is showing the
  operator the exact resulting delta and the compile-budget consequence before
  the accept mutates anything.
- Adaptation: candidates keep their schema (`memory_candidates`, unchanged);
  the value is added at review and accept time, both of which already have
  single choke points.
  - **Review renders the delta**: the `"review"` arm of `/memory`
    (`crates/ka-agent-channels/src/commands/memory.rs:88`) renders each pending
    row from `pending_candidates_for` (`runtime/memory.rs:109`) as a
    bullet-level diff — target file, `+ <text>`, and the entry it lands after
    (from `MemoryStore::entries`, `ka-agent-memory/src/lib.rs:252`) — plus the
    dry-run compile impact (below). Memory entries are one bullet per line, so
    a hand-built bullet diff is exact; no diff dependency. The web review
    inbox already diffs the editor against `memory_facts_text`
    (`runtime/memory.rs:101`, used at `ka-agent-web/src/pages.rs:3521`); the
    chat side reaches parity, and `POST /memory/candidate` rides the same
    accept path unchanged.
  - **Dry-run helper**: `MemoryStore` gains `dry_run_add(file, text, budget) ->
    (DiffPreview, CompileReport)` — snapshot the target file, apply the append
    shape of `add` (`lib.rs:177-214`), compile via the private
    `compile_inner` (`lib.rs:329`) without writing `MEMORY.md`, restore the
    snapshot. The report delta ("compiled 5.9K/6.1K; 1 lower-priority file
    would be omitted") is what the review line shows.
  - **Accept is screen → dry-run → apply-or-rollback**: `accept_candidate`
    (`runtime/memory.rs:154-200`) re-runs `screen_proposal`
    (`ka-agent-memory/src/lib.rs:704`) on the stored text (screening rules may
    have tightened since filing), snapshots the target file, then
    `add` → `refresh_compiled_memory_for` (`runtime/memory.rs:278`) →
    `commit` (`lib.rs:304`). Any failure (git missing, compile error) restores
    the snapshot, recompiles, and refuses with the reason — the operator's
    memory never lands half-applied. `/memory log`'s existing git-missing
    error proves the rollback path is real, not theoretical.
- Acceptance (runtime e2e, `private_sessions.rs` style): `review_shows_diff_and_budget_impact`
  (a pending candidate renders `+ <text>` with its target file and a
  `compiled N/B` line that grows by the bullet's bytes);
  `accept_rolls_back_when_git_fails` (memory repo made unwritable → accept
  refuses, the file bytes and compiled `MEMORY.md` are byte-identical to
  before, the candidate stays pending); `accept_refuses_a_candidate_that_fails_today_screen`
  (a pre-rule candidate whose text now trips `screen_proposal` refuses).
- Contract: `docs/contract/memory.md` §"Memory candidates" — cards show the
  delta + dry-run compile impact; accept = re-screen → dry-run → apply with
  rollback. Removal/edit proposals stay a NON-GOAL: a poisoned removal is a
  §4-shaped deletion vector, and `/forget` stays the only removal path.
- Priority: P0 · effort M · risk 1

### 5 Automation turns excluded from personal memory (B15b)

- Research basis: Devin excludes automation sessions from personal-memory
  read/write to prevent cron→memory feedback loops (https://docs.devin.ai/product-guides/knowledge).
  In Ka a cron is not a separate session: `handle_cron_message`
  (`runtime/intake.rs:308`) submits into the creator's own session and
  partition (`runtime/scheduler.rs:376-392` resolves `fire_principal`), so the
  exclusion is source-scoped: turns whose source is `cron`, `cron:<id>`,
  `briefing`, `trigger` or `trigger:<id>` (`is_scheduled` grows the trigger
  arms, `runtime/events.rs:18`; feature 9's turns carry `trigger:<id>`).
- Adaptation:
  - **Write exclusion**: the `memory_propose` handler
    (`crates/ka-agent-channels/src/tools/memory.rs:19`, private gate at `:22`)
    refuses when the turn's source is scheduled, read via the per-user turn
    context `publish_turn_context` already writes at every submit
    (`runtime/intake.rs:9-26`, `:676`; `turnctx(ctx, "source")`, same mechanism
    `kb.rs:46` uses). One gate line beside the private one; the loop
    (cron proposes what a previous cron read) is closed at the only write path. v3 #6's distillation preset is the deliberate exemption — its cron files candidates by design, so `distill` sources stay allowed (the preset's own operator-gated cards remain the control).
  - **Read exclusion**: the daemon owns the compiled file the engine re-reads
    per prompt (memory.md §Compilation, validation §5). `submit_text_with`
    (`runtime/intake.rs:665`) and the queue-drain submit path
    (`runtime/intake.rs:73-166`) bracket a scheduled turn: when
    `is_scheduled(source)`, install the automation compile —
    `MemoryStore::install_compiled` (`lib.rs:291`) driven by a scope flag on
    `compile_inner`/`scan_files` (`lib.rs:329`, `:450`) that keeps
    `environment.md` + `conventions.md` (machine facts) and drops
    profile/persona/preferences/facts/projects (personal).
  - **Restore on every exit from the bracket, before any drain**: the full
    compile is restored (i) at the cycle's terminal points — the
    `TurnFinished` arm (`runtime/events.rs:569`, which fires before
    `PortEvent::Idle`'s drain and itself calls `rt_maybe_idle`/`spawn_drain`,
    so the restore is ordered first), `PortEvent::Idle` (`:751`), and
    `rt_on_engine_closed`; and (ii) in the **Err arms of both bracketed submit
    paths** — a non-`EngineClosed` `submit_full` failure and the drain path's
    Err (turn marked `interrupted`, no port event ever arrives) restore before
    returning, so an ordinary protocol error cannot leave the lean compile
    installed across the user's later normal turns. Boot already reinstalls
    every user's full memory (`runtime/mod.rs:479-500`), and `ensure_user`
    re-compiles on state creation (`runtime/roles.rs:349`): a daemon killed
    mid-bracket self-heals.
  - Cron-side budget deferral is already quota-aware
    (`runtime/scheduler.rs:406-430`: budget-exceeded occurrence skipped and
    re-armed) and is reused, not rebuilt.
  - **Limits (documented in memory.md)**: only daemon process death inside a
    scheduled span leaves the lean compile until the next boot or memory
    change — degraded answers, never a leak (variants compile from the same
    per-user root); two processes refreshing mid-span race benignly (worst
    case the scheduled turn sees full memory).
- Acceptance: `cron_turn_refuses_memory_propose` (a scripted cron turn calling
  `memory_propose` gets the scheduled-refusal string and no candidate row);
  `scheduled_turn_compiles_without_personal_files` (during the cron cycle the
  engine-home `MEMORY.md` contains the environment bullet and no facts
  bullet); `full_memory_restored_after_scheduled_cycle` (after the cron turn
  settles, `MEMORY.md` is byte-identical to the pre-cycle compile);
  `submit_failure_restores_full_memory` (the scripted engine's `Step::Fail`
  shape at submit time → the next chat turn's `MEMORY.md` is the full compile).
- Contract: `docs/contract/memory.md` §"Stable memory/Compilation" (automation
  variant + restore rule) and §"Memory candidates" (scheduled-source refusal,
  beside the private-session row).
- Priority: P0 · effort M · risk 1

### 6 Skill `triggers:`/`paths:` + turn-scoped tool binding + `/skill-doctor`
- **Research basis:** OpenHands keyword-triggered skills — skills inject when a term is mentioned, not only when explicitly invoked (https://docs.openhands.dev/overview/skills/keyword); Claude Code skills — `paths:` globs inject once per conversation when a matching file is touched (not advertised), turn-scoped `allowed-tools`, `/skill-doctor` showing per-skill context cost + invocation frequency (https://code.claude.com/docs/en/skills). Negative evidence §1: nothing may re-inject across compaction without an explicit budget — see Handoff.
- **Adaptation:**
  - Frontmatter: extend `crates/ka-agent-runtime/src/skills.rs` — replace the line-scanning `read_description` (`:264`) with a `toml`-parsed frontmatter (crate `toml = "0.9"` already a workspace dep; add to the crate); `SkillInfo` (`:42`) gains `triggers`, `paths`, `allowed_tools`. `enable` (`:167`) bounds-checks: ≤ 8 triggers (each `[a-z0-9-]{1,32}`), ≤ 8 paths, ≤ 8 tools; path globs validated with the house matcher `ka_agent_store::model_matches` (`groups.rs:203`, flat `*`-glob, no new dep). Provisioning strips the three ka-agent keys from the copied `SKILL.md`: parse the `---` frontmatter block as a TOML document (`toml_edit`, already a workspace dep — a document-level Value parse preserving comments and ordering), remove exactly those three keys, reserialize; never line deletion and never a typed-struct round-trip (either drops or mangles the operator's other keys). The pinned engine (ka-harness `247e99e`) never sees unknown keys; the operator's source copy keeps them.
  - Provenance join: stripped engine copies carry no triggers, so `enabled()` (`:153`) alone can no longer answer "what can fire". The runtime reads a `resolved()` view: names from `enabled()`, frontmatter joined from the operator library via `source_of` (`:145`) under `user_dirs` precedence. An engine-home skill with no library copy — the `[enabled, no source copy]` restore case (skills.md) — resolves with empty triggers/paths/tools (explicit `/skill` only) and is marked by the doctor. Trigger matching, path matching, and tool binding read `resolved()` only.
  - `triggers:` daemon-side match at the two plain-text submit sites — the direct path `intake.rs:523-524` and `drain_queue`'s inline submit `intake.rs:169-171` (queued items stay plain text per skills.md; match at drain time). New `matched_triggers(text, &resolved)`: case-insensitive whole-word match, all hits, cap 3 skills/turn, name-ordered; `/skill` turns union the explicit name with trigger hits under the same cap. Cron/scheduler submits are exempt (conversational only).
  - `paths:` in the event pump, `runtime/events.rs` `CallStarted` arm (`:334-342`) — extract path-like tokens from the call `detail`, match `resolved()` skills' `paths` globs; a hit parks the name in a `paths_pending` set on `UserState` (`runtime/mod.rs:178`, beside `live` — survives worker reap). The **next** submit attaches it (single-writer rule forbids mid-cycle injection) and writes the once-per-conversation marker to the store meta kv (`meta_set`, `store/src/lib.rs:306`, key `skillpath:<session>:<skill>`) **at attach time** — never at touch time. Restart semantics: a daemon restart between touch and attach drops the in-memory pending set, that touch is forgotten, and the next matching touch re-parks it — no marker was written, so the skill stays eligible; attachment can be delayed by a restart, never permanently suppressed.
  - Compaction (§1): `Compacting { done: true }` arm (`runtime/events.rs:406`) clears that session's attach-time path markers — lazy re-attach on the next matching touch; no eager re-injection, so no unbudgeted bytes cross the port.
  - Turn-scoped tools: `handle_skill` (`intake.rs:546`) reads the skill's `allowed_tools` from the operator's source copy and passes them as the existing `tool_override` arg (`:585`) — `submit_text_with` already intersects with `role_toolset`, never widening (`intake.rs:861-870`, the cron-tools precedent; roles.rs:151). `${SKILL_DIR}` is out: the protocol takes exact hand names only.
  - `/skill-doctor`: new `CommandSpec` beside `"skills"`/`"skill"` (`channels/src/commands/mod.rs:265,275`), handler in `commands/skills.rs`. Per `resolved()` skill: advertised line cost (name+description bytes → `~N tok` estimate — the engine assembles the prompt, so figures are labeled estimates), body KiB (engine copy), attach counts by kind (explicit/trigger/paths), last used, trigger/path echo, `no source copy` marks; plus one standing-cost total. Counts live in the **next schema migration (number assigned in landing order** — other Wave-2 plans may claim the same slot; the `schema_v12.sql` pattern): table `skill_usage(username, skill, explicit, trigger, paths, last_used_ms, PK(username,skill))` + `store/src/skill_usage.rs`, bumped at the three attach sites. Realizes v3-features #10's usage-count half when that lands.
  - Contract: skills.md gains "Frontmatter: triggers, paths, allowed-tools" — keys, bounds, the provenance join (the operator library owns triggers; the engine copy stays ecosystem-clean), matching semantics, attach-time markers + compaction reset, the never-widen rule (points at the intake intersection), doctor output.
- **Acceptance** (chat e2e, scripted speaker — `cli_e2e.rs:1070` style):
  - `skill_triggers_attach_on_mention` — enabled skill with `triggers=["deploy"]`; "how do we deploy this?" acks and `/skill-doctor` shows `1× (trigger)`; an unrelated message leaves counts unchanged.
  - `skill_paths_attach_once_on_touch` — scenario with a `call` step reading `docs/release.md` (`scenario.rs` Call step, `:77`), skill `paths=["docs/*"]`; the next turn attaches it (`1× (paths)`); a second touch does not re-attach.
  - `skill_tools_never_widen` — role allow `["read"]`, skill `allowed_tools=["read","bash"]`; the speaker scripts a `bash` call which is refused at admit (worker-protocol.md — filtered from the model specs) with no side effect, and the turn still completes.
  - `skill_doctor_prices_context` — two enabled skills; `/skill-doctor` lists per-skill `~N tok advertised · XKiB body` plus a standing total; triggerless skills show no trigger echo; a no-source-copy skill shows its mark.
  - Unit: matcher word-boundaries/case/cap-3; glob acceptance; the strip preserves unrelated frontmatter keys verbatim (comments included); marker written at attach + reset on `Compacting{done:true}` (the speaker has no digest step — `scenario.rs:67-99` — so this stays unit-level).
- **Priority:** P0 · effort M · risk 1
- **Handoff (KaPort):** (a) `disable-model-invocation` — the daemon's only lever, `features.disable = ["skill:<name>"]` (`engine_layer.rs:41`), kills the whole skill including `Submit.skills` injection, not just advertisement; an advertisement-only toggle is engine prompt assembly. (b) Post-compaction skill stickiness — eager re-attach of attached skills with an explicit token budget is context assembly (H4-adjacent); the marker reset above is the entire above-port fraction.
- **Follow-up note:** B13 (`$ARGUMENTS`, per-recipe toolsets, cron snapshot-at-creation) builds on this frontmatter + attach plumbing; not planned here.

### 7 A2 Queue manager — editable, reorderable, jumpable queue
- Research basis: opencode's queue manager (`<leader>q`: view, edit, delete,
  send-now on queued messages) https://opencode.ai/docs/cli/keybinds · Goose
  desktop queued cards (visible/editable/reorderable while a turn runs)
  https://goose-docs.ai/docs/guides/sessions/in-session-actions. Ka already has
  the view half: `/queue` list (`commands/session.rs:419`), Telegram card with
  per-item ✕ + Clear all (`docs/contract/telegram.md:95-98`,
  `telegram/channel.rs:1022-1035,1329`), web `/queue` page (`web-ui.md:121-122`,
  `ka-agent-web/src/pages.rs:616-664`), TUI `queued N` (`term/view.rs:118`).
  Missing everywhere: reorder, edit, send-now.
- Adaptation: (a) store — `queue_reorder(session, id, to_pos)`,
  `queue_set_text(id, text)`, `enqueue_front(item)` in
  `crates/ka-agent-store/src/queue.rs`: the pending rows sort by
  `ORDER BY enqueued_ms, id` (`queue.rs:31`), so reorder rewrites the pending
  block's `enqueued_ms` into the new order in one transaction and
  `enqueue_front` inserts at `head_ms − 1` — no schema migration, no new column.
  (b) runtime — `promote_queued`, `edit_queued`, `send_queued_now` beside
  `queued`/`clear_queue`/`remove_queued` (`runtime/config.rs:313-368`);
  `send_queued_now(n)` = promote to head, then when a cycle is live take
  `stop_turn`'s exact abort shape (`runtime/intake.rs:905-956`: `port.abort()`
  + grace + SIGKILL escalation) — the engine never sees a mid-cycle prompt
  (single-writer rule, ka-port.md §4: `drain_queue` is the only submitter,
  `intake.rs:73`); the jumped item is simply the next `drain_queue` head,
  called at the turn boundary (`runtime/events.rs:930`). (c) router — `/queue`
  grows subcommands
  `rm <n> · edit <n> <text> · top <n> · up|down <n> · now <n>`
  (`commands/mod.rs:355` args `[clear]` → `[subcommand]`; `run_queue` resolves
  n→id from `queued()`). (d) Telegram — the `/queue` card rows gain `↑`
  (`qu:<last-12>` → `promote_queued`, 64-byte callback cap as `qx:` today,
  `channel.rs:1022`); edit/now stay command-side (no in-place edit UX on
  Telegram). (e) web — `POST /queue/move · /queue/edit · /queue/now` beside
  remove/clear (`ka-agent-web/src/lib.rs:1074-1079`, `pages.rs:616-664`).
  (f) TUI — Ctrl-Q opens a queue pane (the `<leader>q` analog): the chat app
  owns the runtime in local mode (`apps/ka-agent/src/chat.rs:131`), so the pane
  reads `runtime.queued()` on open; row keys `x` rm · `u`/`d` move · `e` edit
  (the `term/editor.rs` one-line draft) · `↵` now · Esc closes
  (`term/view.rs`,`term/keys.rs`). Contracts touched: `telegram.md` /queue card
  bullet (↑ row), `web-ui.md` route table (+3 POSTs), `terminal.md` key table
  (Ctrl-Q row); and doc catch-up only — `ka-port.md` §1's handle table AND §2's
  normative allowed-command list, and `worker-protocol.md` §1's D→W table, all
  omit three already-shipped ops: `set_effort`, `steer`, `undo_file`
  (`PortSession::{set_effort,steer,undo_file}`, `port/session.rs:274-285`;
  forwarded as ka commands at `port/worker.rs:573-595`): add the three rows /
  list entries, no code change. Same pass corrects ka-port.md §4's
  "`PortError::Busy`" — the enum has no `Busy` variant (`Spawn/Io/Protocol/
  Closed/EngineClosed`, `port/mod.rs:30-50`); reword to the runtime-owned
  single-writer rule.
- Handoff: none — steer verified live end-to-end above; no engine op is needed
  for view/edit/reorder/jump (abort is a port op).
- Acceptance: `queue_edit_reorder_and_jump` (chat_e2e, `busy_messages = "queue"`):
  with a turn running and three messages queued, `/queue top 3` then
  `/queue edit 1 <new text>` change the list order and text; `/queue now 2`
  aborts the running turn (store shows it terminal `aborted`) and the promoted,
  edited item is the cycle submitted next; a final `/queue` render shows the
  new head. TUI Ctrl-Q pane: manual-verification note — the tree has no pty/
  tmux TUI harness (chat_e2e drives the chat binary over piped stdio, line
  mode only), so the pane's keys (`x`/`u`/`d`/`e`/`↵`/Esc, depth drop) are
  verified by hand against the terminal.md key table.
  telegram `queue_card_promotes_one_item`: pressing `↑` on row 2 settles the
  card with the reordered list (`qu:` callback, beside
  `queue_picker_removes_one_item`, `telegram_e2e.rs:2425`). web_e2e: `POST
  /queue/move` then `GET /queue` shows the new order.
- Priority: P0 · effort M · risk 1

### 8 A3 Interrupt keywords (shipped with A2)
- Research basis: Goose's documented heuristic — high `stop, halt, cease,
  quit, end, abort, cancel, wait, hold, pause`; medium `no, wrong, actually,
  instead, nevermind…` only at sentence start; ≤20-char messages act as high
  only. https://goose-docs.ai/docs/guides/sessions/in-session-actions.
  Docs-only evidence (eval caveat): validate the word list against Ka's own
  transcripts before shipping.
- Adaptation: pure matcher `is_bare_interrupt(text)` in `runtime/intake.rs`
  (unit-tested beside `ctx_line`): trimmed message ≤20 chars, case-insensitive,
  exactly one high word + optional trailing punctuation — medium words are
  deliberately NOT acted on (a mid-cycle correction rides today's steer path,
  which A2 documents). Call site: the `Running` branch of
  `handle_message_active` (`intake.rs:418-458`) before the steer attempt — on
  match, emit Notice `⏹ stopped — your message is next`, take `stop_turn`'s
  abort shape (`port.abort()` + grace escalation, `intake.rs:905-956`; works on
  cron/briefing-owned cycles exactly like `/stop` today), and `enqueue_front`
  the text so it becomes the next cycle via the existing drain.
  `AwaitingApproval`/`Stopping` keep today's queue-don't-abort behavior (the
  approval card owns that state). Config: `interrupts` Enum `on|off`, default
  `on`, `Apply::Live`, beside `busy_messages` (`config_keys.rs:128-133`).
  Validate-on-transcripts (the eval caveat, as a step not a promise): before
  default-on, run the matcher read-only over stored `turns.inbound_text` on the
  operator's instance and review the hit list; exact-word + ≤20-char rules must
  show zero matches on benign shorts ("ok", "thanks", "hm") by construction.
- Acceptance: `plain_stop_aborts_and_becomes_next_turn` (chat_e2e, default
  `busy_messages = "steer"`, scripted long turn): inbound `stop` → the `⏹`
  notice, the running turn ends `aborted` (store), the next submitted cycle's
  inbound text is `stop`, and no "steered into the running reply" note fires.
  `medium_and_long_messages_do_not_interrupt`: `wait for the green build`
  (>20 chars) and `no` steer/queue exactly as today. `interrupts_off_restores_steer`:
  with `interrupts = "off"`, `stop` steers (today's behavior).
- Priority: P0 (low rank) · effort S · risk 1

### 9 Trigger model + webhook ingress + preflight gate (C2 core, B14)
- **Research basis:** Devin automations — triggers (schedule/webhook/GitHub label), cheap preflight before spending a turn, "runs as creator", session lineage per schedule, Error status after consecutive failures, concurrency budget with clean degradation (https://docs.devin.ai/product-guides/automations) · Amp's delivery contract — at-least-once, `200 = queued`, idempotency by `event.id` (https://ampcode.com/docs/orbs/automations). GitHub-label triggers are webhook events; one ingress + a filter covers both, so Ka needs exactly two trigger kinds: `schedule` and `webhook`.
- **Adaptation:** staged; each stage ships green.
  - **Stage 0 — ADR-0005 "inbound trigger surface"** (`docs/adr/0005-…`, beside 0001–0004; S, no code): settles per-trigger secret storage (`secret:` refs, one per trigger vs shared `web.api_token`), hook-lane dispatch ordering (its own route arm before the shared bearer gate — per-trigger secret is the auth, never `web.api_token`), regex-filter semantics (match → fire), size caps, the idempotency table, preflight isolation, the public-bind stance (inherits web-relay.md: loopback default `web.bind` `config_keys.rs:506`, doctor warns on `0.0.0.0`, exposure only via reverse proxy/Tailscale), lineage-session policy, and the Error/degradation contract. Nothing else starts before it is accepted.
  - **Stage 1 — trigger model + lineage + identity + error-N + budget** (post-`/watch`): a `TriggerSpec` in `crates/ka-agent-core/src/task.rs` beside `CronSpec` (`:186-226`; same `#[serde(default)]`-tolerant payload pattern so old rows load), stored in the same tasks-table `payload` (`kind = "trigger"`); schedule triggers reuse the `when.rs` grammar (`CronSpec::from_words`, `task.rs:276`). Scheduler gains a `"trigger"` arm beside `"cron"` in `fire_due_tasks` (`scheduler.rs:293-321`). Per-trigger identity reuses the cron rule verbatim — creator `username`/`role` re-resolved from the user row at fire time, never the stale snapshot (`scheduler.rs:376-392`; `rbac.md:154` row extends to triggers). Lineage: the spec pins a session id created per trigger (`Session::new_for_user`, `runtime/mod.rs:880`); a `handle_trigger_message` sibling of `handle_cron_message` (`intake.rs:308`) submits into that session, not the creator's live one — automation noise never pollutes human chat. Error-after-N: consecutive-failure count on the spec (`update_task_payload`, `store/tasks.rs:70`; runs rows already recorded per fire at `scheduler.rs:270-286`); N strikes (default 3) → `interrupted` (paused/resumable, today's pause semantics) + one Error notice; success resets; run history is never deleted (clean degradation). Concurrency: fires enqueue on the lineage session's durable queue (ka-port.md §4); per-trigger in-flight cap 1 — a fire while that trigger still has a queued/running turn records `skipped: previous run still in flight` (`store.queue_len`, `intake.rs:355`) and does not stack; the global `runtime.max_workers` cap + LRU idle eviction (ADR-0003, `config_keys.rs:282`) already bounds engines.
  - **Stage 2 — webhook ingress** (`crates/ka-agent-web/src/lib.rs`): `POST /api/hook/<trigger-prefix>` gets its **own arm in `route()`, matched before the `/api/` bearer lane** (`:914-916`) — inside `api_route` the shared `web.api_token` gate and its `text`-shaped body parse run before path dispatch (`:1364-1381`, `:1399-1410`), so hooks would demand the global token; the per-trigger secret is the hook lane's only auth. It reuses the lane's machinery: constant-time compare (`:1442`), 404 while the trigger's secret is unconfigured (off, not forbidden), body cap 16 KiB default/per-trigger ≤64 KiB, the shared 30/min bucket (`:1383-1393`), regex filter over the normalized body. Idempotency by `event.id` (header or body): new `trigger_events(event_id PRIMARY KEY, trigger_id, seen_ms)` table on the next schema migration (number assigned in landing order; `user_version` migration pattern, `store/lib.rs:99-100,253-262`; `runs` has no unique key today, `schema_v1.sql:75`); insert-or-ignore decides — replays return `200 {"ok":true,"dup":true}` (Amp: 200 = queued, at-least-once). Accepted events arm the trigger row (`scheduler_tick` wakes the loop, `scheduler.rs:77`).
  - **Stage 3 — cheap preflight gate** (no memory/tools before an agent turn): ephemeral worker via `OpenRequest` (`port/env.rs:128-150`) — fresh strand, cheap catalog row, `mode = readonly`, `Submit.allowed_tools = Some(vec![])` (empty allowlist = no tools, `port/session.rs:241`), a scratch engine-home so the compiled MEMORY.md (`runtime/memory.rs:277`) and persona never load; payload rides fenced in an untrusted-data envelope (`<gate-output>` shape, `scheduler.rs:486-493`) — §4: payloads are poisoned, the preflight prompt states data-not-instructions and the turn has no tools to act with. Verdict parsed like `[SILENT]` (`events.rs:767`): `[SKIP]` re-arms/records with zero agent spend, `[RUN]` submits the real turn on the lineage session under the creator's principal. The zero-LLM half already exists: webhooks are regex-gated at ingress (Stage 2); `spec.gate` shell checks (`scheduler.rs:435-471`) cover schedule triggers today.
  - Contracts touched: `cron-and-refs.md` gains a Triggers section; `web-ui.md` API table (`:95-96`) gains the hook row; `rbac.md` cron row (`:154`) widens to trigger kinds. `ka-port.md` unchanged.
- **Acceptance:** `web_e2e.rs`: `webhook_dedupes_by_event_id_and_queues_one_turn` (same `X-Event-ID` twice → one turn, second response `"dup":true`), `webhook_without_secret_is_404` (no per-trigger secret configured → 404 even with the global token present), `webhook_lane_ignores_the_global_bearer_gate` (no `web.api_token` configured at all + valid per-trigger secret → 200 queued; global token presented but bad/missing per-trigger secret → 404/401 — the ordering assertions), `webhook_rejects_oversized_and_unfiltered_bodies` (413 / filtered → `200 {"ok":true,"filtered":true}`, no turn). Runtime tests (cron_regressions style): `trigger_fires_into_its_own_lineage_session` (creator's live session shows no inbound/turn), `webhook_trigger_runs_as_creator_role_not_admin` (member trigger's turn records the member principal; demoting the member re-clamps at fire), `trigger_errors_after_n_consecutive_failures_and_keeps_history` (3 failed fires → `interrupted` + Error notice; `runs_for` still lists all 3; `resume` re-queues), `trigger_skips_when_previous_run_still_in_flight`, `preflight_skip_does_not_spend_an_agent_turn` (scripted-speaker port answers `[SKIP]` → no lineage-session turn, no tokens recorded on the agent row).
- **Priority: P0 · effort L · risk 2** (inbound attack surface + per-trigger identity = security band; ADR is the mitigation).

### 10 B8 Mode-scoped "always" (closing G12)
- Research basis: Gemini policy engine — "allow always" expands only to the current and more permissive modes (https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/policy-engine.md). Internal evidence: ka-port.md §2 — the engine's "always" is a coarse per-tool allow written into the project `.ka/ka.toml` (gap G12), which is why `ApprovalDecision::Always` (core/approval.rs:69-71) is offered by no surface and choice 1 is never sent.
- Adaptation:
  - With mode gating already in B7's matcher, B8 is the decision surface and the G12 closure: `Runtime::decide_always(approval_id, source)` beside `decide_for_session` (runtime/approvals.rs:197) — same gate, terminal and web only, phone refused with the existing wording (commands/approvals.rs:60-64, "a phone grant would outlive the glance it was given with"). It stamps the rule's `mode` with the session's current canonical mode (`normalize_mode`, core/config.rs:808) and answers the engine via `Runtime::answer_allow` (label `Allow (always)`, choice 0 — engine choice 1 stays never-sent, so **no `.ka/ka.toml` write can result**: G12 is closed by owning the persistence, not by forwarding the engine's; the settle is checkpointed like every other allow).
  - Ceilings compose for free: a member's session mode is clamped to the role ceiling (runtime/config.rs:587-622), so a role-born rule can never fire above it; rules are subject-partitioned (subject = acting username), never widening the operator or another role (rbac.md grants partitioning). `/rules` shows each rule's mode scope; `decide_always` audits `approval.always` with the mode.
  - Contracts: ka-port.md §2's G12 sentence updates — durable "always" now lives in Ka Agent's rules layer with mode ceilings; the engine `always` answer remains never-forwarded. grants.md's new section (B7) records the mode semantics.
- Acceptance:
  - `rule_scoped_to_creation_mode_and_looser`: rule born in an ask-mode session; `/mode readonly` → the identical ask raises a card again (rank 3 > 2); `/mode accept_edits` → auto-allowed with the rule note.
  - `phone_always_refused`: a telegram-sourced `/y <id> always` gets the one-line refusal naming terminal/web (unit, mirroring the session-form refusal); no rule row exists.
  - `role_rule_never_above_ceiling`: role ceiling `accept_edits`; the member's rule stays silent when the role session drops to `ask`; the admin's identical ask still raises its own card (subject partition).
  - `always_answers_choice_0_only` (R6 — replaces the vacuous `.ka/ka.toml` file assert, which the scripted engine never writes): a runtime test asserts the settle audit row reads `Allow … choice 0` and `ApprovalSettled` fires; engine choice 1 is never observed.
- Priority: P1 · effort S · risk 2 (approvals surface; delta over B7 is one command form, one gate, one contract sentence — the rank machinery already landed in B7).

### 11 Quota/warm-up-aware proposal timing (B15c)

- Research basis: Codex memories gate extraction on idle eligibility (default
  6 h, config comment recommends >12 h to avoid premature extraction;
  https://exsesx.dev/blog/en/codex-memories) and the digest's Codex cluster
  (skip near rate limit, skip tool-tainted chats). Ka has no post-idle
  extraction moment — proposals are in-turn tool calls — so the idle arm ports
  as a warm-up gate and the quota arms port as handler refusals.
- Adaptation: three refusals in the `memory_propose` handler
  (`tools/memory.rs:19`), after the private and scheduled gates:
  1. **Budget**: `budget_verdict_for(username)` (`runtime/roles.rs:532`) is
     `Warn { .. }` (≥80% of a cap, `ka-agent-core/src/budget.rs:12`) or
     `Exceeded` → refuse "budget window is nearly spent".
  2. **Rate-limit cooldown**: the user's newest turn with
     `error_class = 'rate_limit'` (the §7 error classes ka-port.md defines,
     recorded in the `turns` table's `error_class` column, schema v1) updated
     within 30 min → refuse; new store fn over the existing turns table.
  3. **Warm-up**: the caller's live session has fewer than
     `[memory] extraction_min_turns` recorded turns (new config key,
     `core/config_keys.rs`; default 5, 0 = off). The session id is resolved
     exactly as `session_private_for` resolves it at tools/memory.rs:22 — the
     handler holds the runtime and the turn's username (turnctx); the count is
     a `COUNT(*)` sibling of `latest_turns_for_session` (`store/turns.rs:123`).
  Tool-taint is deliberately NOT a text gate: the existing poison defenses
  (`screen_proposal` — instruction-shaped, credential-shaped, invisible
  Unicode — plus operator cards and B15a's exact-delta review) are the §4
  mitigation and stay the only ones.
- Acceptance: `proposals_refused_near_budget_and_in_rate_limit_cooldown`;
  `early_session_turns_do_not_propose` (first turn of a fresh session →
  refusal; with `extraction_min_turns = 0` the same call files a candidate).
- Contract: `docs/contract/memory.md` §"Memory candidates" — one row naming
  the three refusal conditions.
- Priority: P1 · effort S · risk 0

### 12 C4 early slice — editable plan artifact + daemon-side verify/repair cycle
- Research basis: Jules runs sessions against a plan artifact with `requirePlanApproval`
  gating execution (https://developers.google.com/jules/api; digest §C4: steps,
  assumptions, chat revision); stop-gate (quality check blocks "done") is OpenHands'
  (https://docs.openhands.dev/openhands/usage/customization/repository); failure
  re-feed is aider's lint/test loop (https://aider.chat/docs/usage/lint-test.html —
  formatter footgun documented: the gate runs an operator-chosen command, never a
  formatter); Plandex's bounded `debug 'npm test' [10]` loop: digest §C4.
- Adaptation: (a) **Store** — the next schema migration (number assigned in landing order; baseline ends at v12), appended to `MIGRATIONS`
  (`crates/ka-agent-store/src/lib.rs:101-113`): `plans(id, session, username, title,
  body, verify, max_iterations=3, iterations, status, draft_turn, created_ms,
  updated_ms)`, status ∈ drafting|draft|approved|executing|done|failed|abandoned;
  CRUD + conditional `bump_iteration` in new `store/src/plans.rs`. (b) **Commands** —
  `/plan draft <goal>|list|show <id>|set <id> <markdown>|approve <id>|run <id>|stop
  <id>` in new `channels/src/commands/plans.rs`, one `COMMANDS` row
  (`commands/mod.rs:74`). Reply-based editing everywhere (router is daemon-side; no
  `$EDITOR` suspend): `/plan set <id> <markdown>` rewrites the body (an `approved`
  row flips to `draft`; re-approve to run). (c) **Capture** — the `Idle` arm
  (`runtime/events.rs:751`) already takes the whole cycle text (`live_text`, `:759`);
  for a `plan:<id>` source on a `drafting` row: write body, lift a `Verify:` line
  into the column, flip to `draft`. The empty-answer nudge's re-ask appends to the
  body; still-empty stays `drafting` + Notice. (d) **Chat revision** — bridge tool
  `plan_update{id, body}` (`BUILTINS` row, `tools/mod.rs:96`, the `memory.rs`
  handler shape), `Effect::LocalWrite` (auto-allowed via `host.rs:249-258`); writes
  only `drafting|draft` rows — an approved plan is immutable to the model.
  (e) **Verify/repair** — same `Idle` arm, finished `plan:<id>` turn with status
  `executing`: run the plan's `verify` through the zero-token gate (`run_gate` +
  `GateOutcome`, `scheduler.rs:940-999`: `sh -c`, workspace cwd, 30 s kill, 4 KB
  cap; `spawn_blocking`-wrapped — the async `drive` task never blocks) — `Pass` →
  `done` + Notice; failure → if `iterations < max_iterations`, `bump_iteration` and
  enqueue one repair `QueueItem` (source `plan:<id>`, gate output inside the
  existing `<gate-output>` envelope, `scheduler.rs:486-493`) that drains via
  `drain_queue` (`intake.rs:73`) — single-writer (`ka-port.md` §4) untouched; at
  the cap → `failed` + Notice. A turn ending `error|aborted|interrupted` runs no
  gate, auto-submits nothing (§7.3), stays `executing`. Every gate execution
  appends an audit row (`plan.verify`, outcome + exit) — a local shell (cron-gate
  parity); README's intents rule stays scoped to external writes. `verify = NULL`:
  the approved plan runs gateless, settles `done` on a clean turn. (f) **Security
  line** — `verify` executes only under an `approved` plan; `approve` is
  operator-only, `/plan show` renders the command `unarmed until approve`, and
  approval consents to exactly that string (the gate runs as the daemon, like
  `run_gate`). (g) **Budget/stop/restart** — the cron budget check
  (`budget_verdict_for`, `scheduler.rs:406`) gates each repair (`Exceeded` → stay
  `executing`); `/plan stop` → `abandoned` + `queue_remove` of the pending repair
  (`store/queue.rs:75`); `iterations` is durable; boot finding `executing` + empty
  queue resumes nothing (operator re-runs `/plan run`). Sequencing: after
  `docs/proposals/cycle-state.md` (owns these `Idle` predicates).
- Acceptance (chat e2e style, scripted speakers): `plan_draft_captures_the_answer_as_a_
  draft_artifact`; `plan_draft_nudge_appends_to_the_body`; `verify_gate_pass_marks_the_
  plan_done`; `verify_gate_failure_repairs_then_caps` (cap 2 → `failed`, audit rows,
  empty queue); `plan_stop_removes_the_queued_repair`; `plan_update_tool_refuses_
  approved_plans`; runtime test `executing_plan_survives_restart_without_auto_run`.
- Handoff: the B5 stop-gate (a B4 hook) and B6's same-turn failure re-feed live in
  the turn loop below the port; debug-with-rollback waits on B1 checkpoints
  (feature 2) — the iteration cap is the only bound here.
- Contracts: new `docs/contract/plans.md` (status machine, approval-arms-verify,
  `<gate-output>` envelope, `plan.verify` audit); `ka-port.md` §4 unchanged — a
  repair is an ordinary queued inbound.
- Priority: P1 · effort M · risk 2 (auto-submits follow-up turns; daemon runs an
  operator-approved shell gate)

### 13 C1 scout slice — one read-only research child
- Research basis: Claude Code subagents: per-child model/tool restriction, read-only
  variants (https://code.claude.com/docs/en/sub-agents); digest §C1 adds escaped
  non-authoritative output (Claude), idle-notify (opencode), cap+queue (Goose). §2
  is the requirement set (cold-start duplication, non-inherited permissions,
  hooks-differ, zero quality data), mapped below.
- Adaptation: (a) **Tool** — `scout{question}` in `channels/src/tools/` (the
  `memory.rs` shape) + `BUILTINS` row (`tools/mod.rs:96`) with a new `Effect::Spawn`
  (`tools/mod.rs:33`); `auto_allowed` (`:228`) excludes it, so `host.rs:249-258`
  omits `kagent__scout` from the auto-allow profile — ask mode raises the normal
  permission card (TTL/audit apply); one `HOW_YOU_WORK` line (`runtime/identity.rs:
  9-20`), labeled beside `delegate` (`core/labels.rs:26`). (b) **Child session** —
  `Runtime::run_scout(who, question)` in new `runtime/scout.rs`: an `OpenRequest`
  on the parent's `prepare_open` partitioning (`runtime/worker.rs:173`: same
  HOME/`KA_DATA_DIR`/grants, no new partition) but `strand: StrandSelector::New`,
  `mode: "readonly"`, submit narrowed by `Submit.allowed_tools`
  (`worker-protocol.md:58-61`) to `read, glob, grep, web_search, web_fetch` ∩ the
  role's `role_toolset` (`roles.rs:151`); empty intersection stays `Some(vec![])`
  (`null` = every tool); *stated* non-inheritance: strict subset, never superset.
  The prompt is the question + `ctx_line` (`intake.rs:37`) + read-only instruction —
  not the parent transcript (§2 cold-start). (c) **Lifecycle** — the handler drives
  the child's own event stream (a mini-`drive`, not the `users`-map loop,
  `events.rs:189`): deltas accumulate to a final text, then `close`/`kill` (the
  `stop_turn` shape, `intake.rs:905-955`); deadline `scout.timeout_secs` (**90**
  default — under the `__mcp` relay's 120 s IPC timeout, `bridge.rs:22`, and
  `turn_watchdog.abort_secs`); a wedged child errors, the parent watchdog
  backstops. A child `Ask` is answered deny immediately, fail-closed, with an audit
  row. Each run records a `kind = "scout"` `RunRow` with cost/tokens
  (`store.insert_run`, `lib.rs:118`): §2's no-quality-data answered with
  spend/latency. Daemon hooks (`hooks.rs`) fire on agent events only; the child
  emits none (§2, in-contract). (d) **Escaped return** — the summary truncated to
  `scout.max_summary_chars` (default 6000) under the header `--- scout summary
  (read-only child, non-authoritative — verify before acting) ---`, inner `---`
  normalized; the tool description marks it untrusted (§4: child output may quote
  poisoned content). (e) **Cap + queue** — the spawn holds `sh.spawn_lock` across
  cap check + claim (`worker.rs:87-90`'s hazard): `live_worker_count() +
  scouts_live ≤ max_workers` under the lock; `scouts_live: Mutex<usize>` on
  `Shared` (beside `auto_allow`, `runtime/mod.rs:260`), summed into
  `live_worker_count()` (`worker.rs:66`) so `ensure_worker` (`:95`) counts scouts.
  Scouts are exempt from `evict_lru_idle_worker` (`:450`, mid-research by
  definition); a full pool queues on `Semaphore(scout.max_concurrent)` (default 2)
  to the deadline → busy error. Config `[scout]` (`core/config_keys.rs`):
  `enabled` (**false**), `max_concurrent`, `timeout_secs`, `max_summary_chars`.
- Acceptance (chat e2e, scripted speakers — `scenario_override` covers every open,
  `port/mod.rs:98`): `scout_runs_a_read_only_child_and_returns_the_summary` (non-
  authoritative header + a `kind=scout` run row); `scout_child_gets_the_question_
  not_the_parent_transcript`; `scout_wedge_hits_its_deadline_and_errors`;
  `scout_child_ask_is_denied_fail_closed`; `scout_asks_in_ask_mode_and_denial_
  spawns_nothing`; `scout_respects_the_worker_cap`; unit `scout_tools_subset_of_role_toolset`
  (web-less role drops `web_fetch`; empty intersection → `Some(vec![])`, never `null`).
- Handoff: child-strand context provisioning (bounded parent-context seeding, no
  cold-start duplication) is context assembly below KaPort (H3/H4 family);
  `Submit.text` is today's only channel — this slice passes only the question;
  full C1 (durable inbox, resumable terminal children, per-child model/budget,
  idle-notify) follows once proven.
- Contracts: `mcp-bridge.md` gains the `scout` tool + `Effect::Spawn` semantics (asks
  in ask mode, absent from auto-allow); `rbac.md` gains: a scout runs inside the
  parent user's partition, strict toolset subset.
- Priority: P1 · effort M · risk 2 (spawns workers outside the users map; new
  approval effect)

### 14 B19 Diagnostics bundle (incl. the honest fraction of B18)
- Research basis: Goose's diagnostics bundle — system + conversation + config + recent logs in one JSON for support, with the privacy tradeoff documented (https://goose-docs.ai/docs/guides/); B18's exact model-visible prompt is context assembly (below KaPort) — only the daemon's own layers are exportable.
- Adaptation: `ka-agent doctor --bundle <path>` (`doctor()` at `apps/ka-agent/src/doctor.rs:181`) writes one JSON: the doctor lines (checks, versions, store state), the effective config with `secret:` refs left opaque by construction, live `StatusReport` via the admin socket (`apps/ka-agent/src/ipc/`), the last N turns of each recent non-private session (`list_recent_sessions`, `sessions.rs:92`; private sessions record no text by contract), and the daemon-side prompt layers — compiled identity (`identity.rs:23`), enabled skills, and the user layer text read at worker spawn (`worker.rs` `user_layer`, `crates/ka-agent-runtime/src/runtime/worker.rs:299`) — labeled `daemon layers only; the engine assembles the final prompt`. A privacy warning ("this file contains your conversation and config — share deliberately") prints before writing; the bundle is never sent anywhere (zero-telemetry stance, v3 #11). Absorbs A12: each session entry carries id + strand path for resume-elsewhere.
- Handoff: exact assembled prompt / last API payload export requires engine context-assembly access (`ka-port.md` §H-analog) — note in `docs/handoff/ka-harness-features.md`.
- Acceptance: `doctor --bundle /tmp/b.json` on a fixture daemon produces JSON containing doctor lines, a config whose secret fields show `secret:…`, at least one turn of the seeded session, the compiled identity text, and no turn text for a private session; stdout carries the privacy warning; no network egress exists in the path.
- Value: eval-value's objection ("doctor covers solo; bundle pays at team scale with real support load") sets the trigger, not a rejection — this is a doctor extension that pays exactly when team-edition support load arrives and absorbs A12 while waiting; rides along at P1 (arch lens).
- Priority: P1 · effort M · risk 1

### 15 A14 Agent commit attribution
- Research basis: aider attributes agent commits — `(agent)` author/committer plus a `Co-authored-by:` trailer — making machine-written history auditable at a glance (https://aider.chat/docs/git.html).
- Adaptation: new `[git]` config keys (`name`, `email`, defaults `ka-agent (<username>)` / `<username>@ka-agent.local`) in `crates/ka-agent-core/src/config_keys.rs`. The engine process env is built by `WorkerEnv::sanitize` (`crates/ka-agent-port/src/port/env.rs:96` — env_clear + credential strip + pinned `KA_AGENT_*` vars; the site every spawned engine command passes through) — extend `WorkerEnv` (constructed at `crates/ka-agent-runtime/src/runtime/worker.rs:269`) with `git_name/git_email: Option<(String,String)>` and have `sanitize` set `GIT_AUTHOR_NAME/EMAIL` + `GIT_COMMITTER_NAME/EMAIL` from it, scoped to the workspace user, so any git the engine's shell tool runs is attributed. The `Co-authored-by: ka-agent` trailer cannot be forced via env: the compiled identity preamble (`compile_identity`, `crates/ka-agent-runtime/src/runtime/identity.rs:23`) gains one line stating the trailer convention for commits it makes.
- Acceptance: an ask-mode turn approved to commit produces a commit whose author and committer are the configured agent identity (`git log --format='%an %ae'`); with `[git]` unset, behavior is exactly today's; a second user's turn commits under their username, not the operator's.
- Priority: P1 · effort S · risk 1

### 16 B13-slice Cron snapshot-at-creation (immutability)
- Research basis: Goose's scheduler COPIES the recipe at creation so jobs are immutable — later recipe edits never silently change running jobs (https://goose-docs.ai/docs/guides/recipes/recipe-reference); negative evidence §5: upgrade-path behavior change is the top churn driver.
- Adaptation: today a briefing job stores only the placeholder `text: "daily briefing"` (`enable`, `crates/ka-agent-runtime/src/briefing.rs:36-56`) and the fire path re-reads the live `briefing::PROMPT` constant (`crates/ka-agent-runtime/src/runtime/scheduler.rs:474-486`) — every daemon upgrade silently rewrites what existing jobs ask the model. Fix: `enable` bakes the full `PROMPT` (briefing.rs:16) into `spec.text`; the scheduler fires `spec.text` unchanged (only `briefing_context` — time data, not instructions — stays fire-time); a one-time boot migration snapshots the current `PROMPT` into legacy `source == "briefing"` payloads, logged as one line. `docs/contract/cron-and-refs.md` (Cron section) gains: "a cron job runs the text snapshotted at creation; daemon upgrades affect only new jobs."
- Acceptance: create a briefing, edit the `PROMPT` const in a test build, restart: the existing job fires its creation-time text (asserted on the submitted prompt), a newly enabled job fires the new text; `CronSpec` payload roundtrip keeps the full text (`cronspec_payload_roundtrip` pattern, `crates/ka-agent-core/src/task.rs:365`).
- Priority: P1 · effort S · risk 1

### 17 Small affordances batch

#### A6 Rate-limit cooldown surfaced in status
- Research basis: Gemini v0.68 shows the rate-limit cooldown countdown in the spinner/status instead of a bare error (https://github.com/google-gemini/gemini-cli/blob/main/docs/resources/quota-and-pricing.md).
- Adaptation: the `PortEvent::EngineError` arm (`crates/ka-agent-runtime/src/runtime/events.rs:936`; `rate_limit` class mapped at `crates/ka-agent-port/src/port/events.rs:222`) today only emits an `AgentEvent::Error`. On `class == "rate_limit" && retryable`, also stamp `throttled_since_ms` into the runtime's shared state; `StatusReport` (`crates/ka-agent-runtime/src/status.rs:7`, `render` at `:147`) gains `throttled_since_ms: Option<i64>` and renders `⏳ rate-limited since HH:MM — retrying in-engine`; the TUI status line (beside `ctx {pct}%`, `apps/ka-agent/src/term/view.rs:106`) shows `⏳ rate-limited` while set; the next settled turn clears it. The JSON surface is the CLI, not the slash command: `/status` takes no args and renders text only (`run_status` at `crates/ka-agent-channels/src/commands/session.rs:8`, `args: ""` at `commands/mod.rs:96`), while `ka-agent status --json` (`status_client`, `apps/ka-agent/src/daemon.rs:148`; admin-socket arm at `apps/ka-agent/src/ipc/server.rs:185`) serializes `StatusReport` verbatim — the new field rides it with no extra plumbing. No fake countdown: the engine's retry-after is below KaPort, so the line shows elapsed time only (handoff note for `retry_after` on `EngineError`).
- Acceptance: with a scenario port emitting a `rate_limit` `EngineError` (the shape at `crates/ka-agent-port/src/scenario.rs:450`), `ka-agent status --json` reports `throttled_since_ms`, the rendered `/status` text and the TUI status line show `⏳ rate-limited`, and a subsequent successful turn clears the field.
- Value: eval-value scored A6 backlog ("fold into v3 #3's status line if free") — exactly that fold: one field + one render arm landing inside v3 #3's status line; rides along, P2.
- Priority: P2 · effort S · risk 0

#### A8 `/presets` — numbered model+mode+effort slots
- Research basis: Codex `/presets` — numbered slots storing model+mode+effort combos, one keypress to apply (https://developers.openai.com/codex/developer-commands).
- Adaptation: the pickers already exist and resolve by number (`/mode`, `/effort` menus at `crates/ka-agent-channels/src/commands/session.rs:71-83`; `/model` listing + `model_header` at `:84`). Add `/preset save <1-3>` capturing the live triple (model via `Runtime::model_for`, mode, effort) per user — persisted in a new `presets(username, slot, model, mode, effort)` store table on the sessions-migration pattern (`crates/ka-agent-store/src/sessions.rs`), username-partitioned like `sessions`. `/preset <n>` applies it through the existing setters (`set_model`/`set_mode`/`set_effort` at `crates/ka-agent-runtime/src/runtime/config.rs:508,558,247`), which already enforce model ACLs and role mode ceilings — a preset may not lift either. `/preset` alone lists slots.
- Acceptance: `/preset save 1` under `ask` + a member model, then switching away and `/preset 1` restores both (`model_header` shows the model, `/status` the mode); saving a slot whose model the user's groups disallow is refused at apply time with the `/model` refusal wording; another user's slot is never visible.
- Value: eval-value's objection ("profiles + /mode already cover most combos") holds for policy — presets add none, they compose the existing setters and inherit model ACLs + mode ceilings; the delta is compressing a 3-command phone round-trip into one. Rides along, P2.
- Priority: P2 · effort M · risk 1

#### A9 Ctrl-L as a true visual clear
- Research basis: Gemini separates visual clear (Ctrl-L) from `/clear`'s fresh session, preventing accidental context wipes (https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/commands.md).
- Adaptation: Ka already has the split — `/new` starts a session, and Ctrl-L (`apps/ka-agent/src/term/mod.rs:230-233`) claims "clear the screen, keep the live area" but only prints `\x1b[2J\x1b[H` raw: the cursor homes and the live area (activity, status, draft) is not repainted, so the next `redraw`'s `cursor_up` bookkeeping (`apps/ka-agent/src/term/view.rs:156-171`) starts from a blank screen. Fix: route the key through the paint pipeline — add `Op::ClearScreen` (`\x1b[2J\x1b[H`, `apps/ka-agent/src/term/paint.rs:13`) followed by the full `redraw_ops` list, inside the existing sync-begin/end wrapper; session state is untouched by construction.
- Acceptance: unit test beside the existing paint/redraw tests (`apps/ka-agent/src/term/`): the Ctrl-L handler's op list starts with `Op::ClearScreen` followed by exactly the ops `redraw_ops` (`view.rs:164`) yields for the live view (activity + status + draft, sync-wrapped), and `Op::render` output for the sequence emits `\x1b[2J\x1b[H` before the repaint; a property check asserts no session/queue state method is reachable from the handler — `/new` stays the only context-wiping path.
- Value: eval-value called this "TUI cosmetics" — granted, but it is a correctness fix of an existing key whose comment promises "keep the live area" while the code drops it; rides along, P2.
- Priority: P2 · effort S · risk 0

#### A13 `/archive` — hide sessions, keep transcripts
- Research basis: Codex `/archive` hides sessions from lists without deleting the transcript; `unarchive` restores (https://developers.openai.com/codex/developer-commands).
- Adaptation: `Session` (`crates/ka-agent-core/src/session.rs:63`) gains `#[serde(default)] pub archived: bool`; a `archived` column rides the next store schema bump (`crates/ka-agent-store/src/schema_v1.sql` precedent, `sessions.rs` `SESSION_COLS`). `/archive` archives the caller's live session (after `/new`-style confirmation on phone surfaces it is one command, no prompt); `/archive undo <id-prefix>` restores. `list_recent_sessions` (`crates/ka-agent-store/src/sessions.rs:92` — feeds the web session list at `crates/ka-agent-web/src/pages.rs` and retention bookkeeping) takes an `include_archived: bool`, defaulting off; transcripts, turns and usage rows are never touched. Archived rows stay visible to `/archive list` for undo.
- Acceptance: after `/archive`, the session disappears from the web session list and `list_recent_sessions` (but its turns remain queryable by `@turn:`/`/history`); `/archive undo <prefix>` returns it to the list; another user's session id prefix is refused.
- Value: eval-value's "list hygiene" objection is granted as stated; the driver is the web session list growing unboundedly for an always-on daemon. Rides along, P2.
- Priority: P2 · effort S · risk 0

#### A16 Published config JSON Schema
- Research basis: Gemini publishes a hosted raw schema URL so editors autocomplete the config file and drift dies at typing time (https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/configuration.md).
- Adaptation: `ka-agent config schema` already exists (`ConfigCommand::Schema { json }` at `apps/ka-agent/src/config_cli.rs:20`, dispatched at `:305`, `fn schema` at `:311`) printing one row per registered key, `--json` the registry as JSON — the `unknown_key` hint points operators at it (`:28`), so that output is a contract and stays byte-identical. Extend, don't replace: `schemars` (already in the lock graph) becomes a direct dep of `ka-agent-core` and `#[derive(JsonSchema)]` rides the existing serde derives across the `DaemonConfig` tree (`crates/ka-agent-core/src/config.rs`) — 69 `KeySpec` entries at HEAD (`config_keys.rs`) documented once and cross-checked against the derived schema (a drift test fails when a registered key is missing from `JsonSchema`). New flag `config schema --json-schema` prints the draft-07 document; the generated file is committed at `docs/schema/ka-agent.schema.json` with a raw URL in the config docs and README, so `"$schema"` at the top of `ka.toml` gives editor autocomplete. The TOML loader tolerates the `$schema` key (ignored, like comments) — one parser test.
- Acceptance: `config schema` (no flag) and `--json` output is unchanged from today (golden test on the existing rows); `config schema --json-schema` emits a schema that validates a fully-populated fixture config and rejects an unknown key; a dev script regenerates the committed file and a CI diff fails on drift; `"$schema"` in `ka.toml` loads without a warning.
- Value: eval-value's objection ("setup-time only; wizard + strict config carry it") misses that strict config rejects after the fact — the schema kills the typo at typing time, deleting a permanent support class; worth its M only as the committed artifact + drift test (arch lens ranked it plan-now).
- Priority: P2 · effort M · risk 0

### 18 `/history` filters: model:/after:/status: (B17 power-user half)

- Research basis: Amp thread search filters (`label:/parent:/after:/before:`;
  https://ampcode.com/docs/threads). eval-value rated this backlog ("serve
  power users, not the two real personas"); it ships here as the cheap P2
  rider on the FTS machinery, with no dependency on feature 3.
- Adaptation: `run_history` (`commands/memory.rs:142`) parses filters **after**
  the `show ` strip and only in **leading position** — `model:<id>`,
  `after:<YYYY-MM-DD>` (parsed in the profile tz via jiff) and
  `status:<queued|submitted|running|done|error|aborted|interrupted|killed>`;
  an `after:` token mid-query stays ordinary searchable text.
  `search_episodes` / `search_episodes_for_user` (`store/turns.rs:243`,
  `:328`) take the parsed `EpisodeFilter` and add `AND t.model = ?`,
  `AND t.created_ms >= ?`, `AND t.status = ?` on the `turns t` join they
  already perform; filter values bind as SQL parameters and never enter the
  `fts_and_query` MATCH expression (verified sound by Review B). With a
  filter, the LEFT JOIN is effectively inner — episodes whose turn row was
  pruned silently drop under filtering; that difference is accepted and
  asserted. Filters with no FTS text (`/history status:error`) run a
  newest-first turn listing with the same predicates, scoped with the same
  admin/user split `history_search` uses (`runtime/memory.rs:228-239`).
- Acceptance: `history_filters_model_after_status` (`/history model:<id>
  after:<date> <terms>` returns only matching turns; `/history status:error`
  with no text lists the caller's error turns and the admin's listing spans
  users; a user's filtered search never returns another user's hits).
- Contract: `docs/contract/memory.md` §"Episodic memory" — filter grammar
  line (leading-position, bound predicates).
- Priority: P2 · effort S · risk 0

## Staged follow-ups (sequenced by the evaluators, not planned here)

- **C5 stage 2 — typed grant language + tighten-only policy layer** (Cursor
  `Shell(cmd)/Read(glob)/WebFetch(domain)/Mcp(s:t)` grammar, glob-deny, protected
  paths inside writable roots, opencode tighten-only with inverted precedence,
  Gemini admin-always-wins tiers). Only after rules stage 1 (features 1 and 10) proves the
  shapes; needs its own ADR rewriting `grants.md`/`rbac.md`.
- **B4 hooks + hash-trust** (Codex/Gemini/Goose lifecycle events, trust-by-content-hash,
  strict JSON, fail-closed) with **B5 stop-gate** as first consumer. Contract doc before
  code; neg-evid §3 requirements are the design brief.
- **C3 rest** — worktrees per session/task (B3) and `/best-of-n` after team M1 beds in;
  B2 pending-changes staging stays below-port (handoff).
- **C1 full addressable-agent machinery** — after the scout slice proves the
  mitigations; Warp's BLOCKED-state durable inbox is the reference shape.
- **B16 MCP elicitation + `@server://resource`** — @resources is a small refs extension
  (do when cheap); elicitation maps to the existing `Ask` but needs the engine to
  suspend a tool mid-call (design with engine).
- **A15 `AI!` file-comment watcher** — builds directly on v3 #1 `/watch` once it lands.
- **C6 opt-in observability export** — pulls in when team-roadmap M3 dashboards want
  the data; OTel decision dimensions (approved/denied × config-vs-human, approval
  latency, compression before/after) are the spec.

## Backlog (evaluator one-liners; no plans)

A4 `/btw` (needs engine side-strand) · A11 notification tray (after watchers generate
volume; parent-only rule recorded for C1) · A12 (absorbed into the diagnostics bundle) ·
A15 (above) · B10 autonomy ladder (modes/ceilings already form one; renaming adds
nothing) · B13 full recipes ($ARGUMENTS after feature 6; snapshot slice planned) ·
B16 (above) · C6 (above).

## Deferred with prerequisites

B2 staging (needs B1 base; below-port anyway) · B6 same-turn repair re-feed
(engine turn loop; approximated daemon-side in the verify cycle) · C3 (above) ·
A1 full edit-and-fork (needs H2 strand fork; composer note rides B1).

## Rejected

No new outright rejections beyond v3's (marketplace, group chats, email connector,
calendar write-back, first-party imagegen, heartbeat-in-main-chat) — the catalog was
pre-filtered against them. Two demotions worth recording: C6 near-term (observability
does not deepen capability) and B10 standalone (flexibility theater over modes).

## Handoff (below-port, for docs/handoff/ka-harness-features.md)

- **H2 strand fork/branch on rewind** — unlocks A1's edit-and-fork, B1's conversation
  half, C3's 3-way rewind; port needs a fork op beside open/submit.
- **H4 compaction budgets + sticky notes** (Plandex never-dropped notes) — what
  survives compaction defines memory; amplifies the B15 features and Ka's differentiator.
- **H1 steer** — SHIPPED at the pin (see drift note); only the doc catch-up remains.
- **H3 tree-sitter repo map** — context assembly below the port.
- **H5 code-interpreter tool batching** (Goose Code Mode) — engine execution model.
- **H6 editable compaction prompt / post-turn compaction threshold** (Codex) — engine
  knob; cheap and complements H4.
- **Side-strand / context fork** — the missing primitive behind A4 `/btw`.
- **B18 exact model-visible prompt** — only the daemon's own layers are exportable
  (in B19); the exact assembly is below-port.
