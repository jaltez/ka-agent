# ka-harness integration — pin `247e99e` (v0.7.0+) → v0.12.2 line

Status: 2026-11-27. Everything below marked "landed" is in the working tree
and verified against the new pin (`f41ecf9`); the pin itself is final.
Inventory of what the 94 commits between the pin and
ka-harness `3a1c031` (v0.12.1) mean for Ka Agent, plus the decisions taken
on them and what enters this round. Companion to `v3-features.md` /
`v4-features.md` (same boundary rule: above `KaPort` is ka-agent work).

## Method

- ka-harness read at `3a1c031` (v0.12.1) and, for the hooks posture, at
  `3db0550` (v0.12.2). "First version" follows the commit order: batches
  1–11 → 0.8.0, batches 12–13 → 0.9.0, `Inventory.skills` → 0.10.0,
  performance → 0.10.2, compact TUI / colour depth → 0.11.0, security
  hardening and true first-match-wins → 0.12.0.
- The re-pin was measured, not guessed: a scratch copy of the workspace
  with the five `ka-*` crates as `path` dependencies on v0.12.1 compiled
  with `cargo check --workspace --all-targets` (default and
  `--all-features`) after seven minimal patches, and
  `cargo test -p ka-agent-port --features test-support` passed (61 unit,
  7 `worker_e2e`). The rest of the suite was not run against the new pin.
- Each domain was read by a scout with `path:line` evidence; the items
  marked *verified* were re-read directly.

Routes: `cfg` = a `Config` key in the worker's layer · `cmd` = a `Command`
to forward (needs a `WorkerCommand`) · `ev` = an event to consume · `hand`
= a tool the model can call · `feat` = cargo feature · `lib` = a public
function of an already linked crate · `cli`/`tui` = only in the `ka`
binary or `ka-term`, unreachable without spawning it.

## Decisions

| # | Question | Decision |
|---|---|---|
| 1 | `__guard` is a `pre_tool_use` hook and nested voices ran no hooks | Fix in ka-harness: child voices inherit the parent's posture. Landed in `3db0550` (v0.12.2): tool-call hooks and `[[rules]]` reach pathfinder, delegate, `/tan`, team members and `code`-routed calls. |
| 2 | New hands at the re-pin | Only `tool_search` stays on; `cron`, `monitor`, `team`, `checkpoint`, `generate_image` are disabled with `tool:<name>` in the worker overlay. |
| 3 | `` !`cmd` `` lines in a `SKILL.md` run `sh -c` at skill load | Reject the skill with a clear error at provisioning, import and proposal-accept; never attach one already present in an engine home. |
| 4 | Several engine features are inert without `[roles] fast` (`auto_review` was a silent no-op before the re-pin) | Compose `[roles] fast` from `background_model` when it is set; `doctor` says so when it is not. With a fast role present `approvals.auto_review` is now live. |
| 5 | Asks from unattended turns (cron, briefing, triggers) park a card for the TTL | Additive `Command::Prompt` field `prompt_policy: Option<"ask"\|"deny">` for that cycle only, added on the ka-harness side by the other session in a release after v0.12.2 (it is not in `f41ecf9`); the daemon then sends `deny` for scheduled sources. Until then unattended asks keep the card and the TTL. |
| 6 | Mid-size integrations | Now: hooks v2 (new events, `timeout_secs`) and `open_engine` assembling `Catalog::merged_embedded_with_runtime` over the runtime catalog. Not done: replacing the local models.dev merge. At `f41ecf9` `load_runtime_catalog` ignores a file whose version is not newer than ka-dialect's own, `fetch_models_dev` returns raw api.json, and the only converter is the unpublished `xtask models-sync`, whose policy drops the vendors the local registry adds — an upstream ask (a public models.dev → catalog refresh with a URL override). Later: `headers_helper`, `net_allow_domains`, `ka-passport`, `[virtual_models]`, code mode. |
| 7 | Context / session commands | Now: `SessionDiff` (`/diff`), `ContextBreakdown` (`/context`), `SideQuestion` (`/btw`), idle recaps, `export --html`, strand `gc` with a retention policy. Later: `ForkStrand` (needs a new session row). |
| 8 | Small config / sandbox tweaks | Now: `[features] allow` composed from `role.allow`, process group + tree kill in the daemon gates. |
| 9 | Order and pin target | ka-harness first (the other session owns that repo's work and release). The re-pin goes to v0.12.2 (`f41ecf9`, on GitHub) now; the per-turn field gets a short second re-pin when it is published. |
| 10 | Coordinating two sessions | The harness working tree belongs to the other session; this session never edits it. |

Defaults taken without asking (say so to change any): the `[fallback]`
chain is filtered by the role's model ACL; `[permissions] profile` is
refused in the operator layer (profile rules sort ahead of the approval
profile); the A6 rate-limit stamp keys on `class` alone; provider keys
stay in the per-user `.env` (the OS keyring is machine-global and would
lose per-profile isolation).

## A. Adapt at the re-pin

### Compile breaks (verified; all in `ka-agent-port`)

| Site | Change upstream |
|---|---|
| `worker.rs:375` literal `Hook` | + `timeout_secs`, `async_`, `url` (no `Default`) |
| `worker.rs:420` literal `McpServerConfig` | + 10 fields (filters, approval, timeouts, oauth, `bearer_token_env`, `headers_helper`) |
| `worker.rs:290` `mode_word` | `Mode::DontAsk` added → non-exhaustive match |
| `scenario.rs:382` literal `SpeakRequest` (test) | + `service_tier` |
| `engine_layer.rs:451` (test) | `Config::effective_search()` returns `Vec<SearchProvider>` |
| `engine_layer.rs:453` (test) | `SearchProvider.api_key_env` is `Option<String>` |
| `engine_layer.rs:471` (test) | `tools.mcp.discovery` is `Option<McpDiscovery>` |

### Behaviour changes that need a decision

| Change | Evidence | Action |
|---|---|---|
| New hands appear without code (`checkpoint`, `cron`, `monitor`, `tool_search`; `team` with `team_max` 8) | `hands/mod.rs:569-590`, `engine.rs:1098-1120` | Decision 2 |
| `[features]` accepts bare `cron`, `monitor`, `generate_image`; the Agent's duplicated validator rejects them | `ka-protocol/src/lib.rs:91-93`, `ka-agent-core/src/config.rs:168-190` | Extend the list or delegate to `ka_engine::features::parse_specs` |
| `[[rules]]` true first-match-wins; profile rules sort ahead of `[[rules]]` | `config.rs:22-35`, `engine.rs:813-818` | Refuse `[permissions] profile`; re-run the grants tests |
| `[[search]]` is a fallback chain; `engine_layer.rs:136` replaces it with one row | `config.rs:665-676` | Append instead of clobber |
| MCP elicitation rides `Event::Ask` with arbitrary options; `ka_choice` fixes deny = 2 | `mcp.rs:2021-2111`, `core/approval.rs:76-83` | Decide by ask shape (this round, pin-independent) |
| `` !`cmd` `` lines in skill bodies | `voice.rs:2712`, `4985-5050` | Decision 3 (this round) |
| `Mode::DontAsk` | `voice.rs:4294-4310` | Arm in `mode_word`; not added to the Agent's mode vocabulary |
| Hooks v2: 7 events, HTTP handlers, `async`, `timeout_secs` | `config.rs:43-175` | Decision 6 |
| Landlock `/dev` loses `/dev/shm` | `ka-sandbox/src/lib.rs:244-252` | Document `allow_write` |
| MCP `approval = "auto"` on `kagent` skips the daemon's `Effect` gate | `voice.rs:1252-1293` | Never set it |
| Terminal `Event::Error` carries `retryable: false` | `voice.rs:3108-3113` | A6 keys on `class` |

## Landed in this round

- Re-pin to `f41ecf9` (v0.12.2) with the seven compile sites, the hand
  overlay, the `[features]` grammar, `[[search]]` appended instead of
  replaced, hooks v2 (seven events + `timeout_secs`), `[permissions]
  profile` refused, `[roles] fast` from `background_model`, `[fallback]`
  filtered per role (also the operator's chain), `[features] allow` from
  `role.allow`, `recap.idle_minutes`, and the runtime catalog in
  `open_engine`.
- Worker ops `session_diff`, `context_breakdown`, `side_question`, with
  typed projections, `spill_path`, and `/diff`, `/context`, `/btw` on
  every surface. A `/diff` answer attaches the whole diff through the
  session's attachments.
- Idle recaps stored per session and shown by `/status`
  (`recap <HH:MM>: …`); private conversations keep none.
- `ka-agent transcript [<session>] [-o PATH]`: one session's transcript as
  the self-contained HTML page (`ka_strand::render_html`).
- Strand sweep: `retention.strands_days` (0 = keep forever) with the keep
  rule — a live worker, a turn in the store or a queued message keeps the
  strand; snapshot journals follow only as orphans.
- `doctor` warns when `auto_review` / idle recaps are on with no fast
  role; the terminal card shows `s allow for this session` only on a
  permission ask.
- Asks answered by shape (permission triple vs MCP elicitation), a TTL
  expiry deadlock in `expire_approvals` fixed, gates kill their whole
  process tree, shell-line skills refused at every entry point, contract
  docs and `examples/config/engine.toml` corrected.

Open: the per-turn ask policy (needs the field the other session adds);
the models.dev registry replacement (blocked upstream, decision 6);
`ForkStrand`; the later list in §D.

## B. Modernize / retire

| Gap | State upstream | Action |
|---|---|---|
| G1 serve/ACP run `Config::default()` | closed in v0.7.2 | docs only; the worker keeps composing from `default()` |
| G5 serve ids | closed (hex ids) | none |
| G8 per-cycle persistence | closed (one write + fsync per turn, 0.10.2; 64 MiB record cap, 0.12.0) | fix stale comments; keep record-before-submit and `DRAIN_BUDGET` |
| G9 interrupted turn | closed (engine synthesizes on reattach) | drop duplicate synthesis if any |
| G14 steers never consumed | closed | drop "steer may be ignored" caveats |
| `ka-port.md` claims `Unknown{raw}` | the reader skips undecodable lines | fix the contract |
| "swap to `ka serve` once it qualifies" | still no control routes through 0.12.x | remove |

Still open, workarounds stay: G3 (no engine ask timeout), G4 (a mid-turn
`Prompt` is dropped; `Command::Queue` is in-memory), G7, G12, G13, G16,
F-1 (`reassert_policy`), F-2.

## C. Replace

| Agent feature | Native upstream |
|---|---|
| local models.dev merge (`registry.rs`, `models_live.rs`) | blocked upstream — see decision 6; the runtime catalog does reach `open_engine` |
| console-URL table (`catalog.rs:44-62`) | `Dialect.doc_url` |
| static `secret:` MCP headers | `headers_helper` (later) |
| N prepended allow rules for `kagent__*` | `approval = "writes"` + `annotations.readOnlyHint` (later) |
| `IDENTITY.md` per worker | `[prompt] identity_inline` (later) |
| H3 tree-sitter repo map (unbuilt) | `[context] prewalk` (later) |

Not replaceable: `__guard` (harness protected paths are write-only, cover
ka's roots and are ask-class; the guard owns reads, ka-agent's roots,
per-user grants, the install gate and pre-approve).

## D. New

Entering this round: `/diff`, `/context`, `/btw`, idle recaps
(`recap.idle_minutes`), `export --html` (`ka_strand::render_html`),
strand `gc` (`retention.strands_days`), `[features] allow`, hooks v2,
the runtime catalog in `open_engine`, `[fallback]` filtered per role.

Waiting on ka-harness: the per-turn ask policy (`Command::Prompt.prompt_policy`).

Later: `net_allow_domains` / `net_deny_domains` proxy, macOS seatbelt,
`[shell] env`, `dontAsk` / `prompt_policy = "deny"` per worker, named
permission profiles, `ka-passport`, `[virtual_models]`, `service_tier`,
local discovery `overlay_discovered`, MCP per-server policy and
resources, rich `read` (PDF, archives), code mode, `TodoItem.confidence`,
`color_depth`, ask-diff line selection, compact terminal layout.

## E. Keep (do not adopt)

`cron` hand, `/loop`, `/goal`, `ka schedule`, `ka action init`
(session-scoped, in memory, no principal, budget or run rows) · `monitor`
and `jobs` (escape cancellation; the reason for `background_after_ms = 0`)
· `ka serve`, `ka acp`, `ka run --rpc` (no per-user isolation, no command
allowlist, no exit 3, no per-submit `allowed_tools`, no scenario speaker)
· `Command::Queue` · `remember`, `MemoryConsolidate`, the memory inbox ·
`ka-index`, `sessions_search`, `ka mcp-serve` · `[roles]`, `[[model_pins]]`
versus the per-user ACL · strand `archive`/`retitle`/`set_pinned` (two
authorities) · `checkpoint`/`RestoreCheckpoint` versus shadow-git ·
`delegate`, `teams`, `.ka/agents` versus `scout` · MCP OAuth, `sampling`,
`bearer_token_env` · `ka-term` as renderer · CLI-only imports and
`ka install` · `generate_image` · `--profile`, `--add-dir`, `KA_<KEY>`,
`ka config set/get` (live in the `ka` binary; the worker calls
`Config::parse_layer` directly).

## F. Handoffs from v3/v4 against upstream

H1 steer: shipped at the pin · H2 fork: `Command::ForkStrand` exists (was
never blocked upstream; the Agent must create a new session row) · H5 code
mode: shipped (0.9.0) · A4 `/btw`: `Command::SideQuestion` ·
`disable-model-invocation`: shipped (0.8.0; stop stripping the key — but
the engine's `paths:` has other semantics, keep stripping that one) · B16
elicitation: shipped, with the ask-shape fix · B13 recipes and C3
worktrees: CLI/hand (0.9.0) · H6: `SetAutoCompact` · H3: `prewalk` · H4:
partial (`ContextEdit`) · B18: partial (`ContextBreakdown`) · `retry_after`:
absent.

## G. Security findings

1. Nested voices ran no hooks (pre-existing via `pathfinder`; multiplied by
   `delegate`, `tan`, `teams`, `code`) — closed upstream in `3db0550`.
2. Child spend is not metered: the daemon counts only the parent
   `TurnFinished.usage` (`runtime/events.rs:806`).
3. Role toolset is not inherited by team members (`allowed_tools: None`).
4. `[[fallback]]` is profile-wide, not role-filtered; a 429 or auth error
   switches model with only a `Note` (`voice.rs:3053-3098`).
5. `approvals.auto_review` was a no-op without `[roles] fast` — decision 4.
6. `KA_SAFEMODE`, `KA_TTY`, `KA_DEBUG_SETTLE` are read in-process by
   ka-engine and survive `WorkerEnv::sanitize`; safe mode also strips the
   guard hooks. Keep to the five pinned variables.
