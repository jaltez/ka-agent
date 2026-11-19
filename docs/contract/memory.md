# Memory model contract — M3

Status: implemented, 2026-10-15, verified by e2e (scripted engine through
the real binary). Three tiers (SEED.md §4.1) with one writer rule: only the
operator accepts stable-memory writes.

## Stable memory (`ka-agent-memory`)

- **Format**: Markdown files under `<data>/memory/` —
  `profile.md persona.md preferences.md environment.md conventions.md
  facts.md projects/<slug>.md` — each entry is one bullet + one provenance
  HTML comment:
  ```
  - prefers concise Spanish answers in Telegram
  <!-- id: mem_1a2b…; source: operator; accepted: 2026-10-15T14:03:11Z; valid_until: - -->
  ```
- **Git backing**: `<data>/memory/` is a git repository (initialized on
  first open). Every accepted change is a commit (`memory: accept <id>`,
  `memory: forget <id>`). The compiled `MEMORY.md` is gitignored (derived
  state). `git log` is the memory audit trail.
- **Compilation**: `MemoryStore::compile(budget)` concatenates files in
  priority order (profile → persona → preferences → environment →
  conventions → projects → facts) under a `# Ka Agent memory` header,
  stripping provenance comments, enforcing `DEFAULT_BUDGET_BYTES = 6144`.
  Files that don't fit are truncated entry-by-entry; later files are
  replaced by a one-line omission marker. The result is written to
  `<engine-home>/.config/ka/MEMORY.md` (ka's user-tier memory, re-read per
  prompt — validation §5: no worker restart needed).
- **Automation variant** (B15b): turns a schedule started (`cron`,
  `cron:<id>`, `briefing` sources) run against an `Automation`-scope
  compile that keeps only the machine facts (`environment.md` +
  `conventions.md`) — profile, preferences, projects and facts are
  dropped, so crons neither read nor feed on personal memory. The daemon
  installs it right before the scheduled submit and restores the full
  compile at every exit of that bracket: the cycle's terminal points
  (`turn_finished` before any drain, `idle` before any drain,
  engine-closed) and the Err arms of both bracketed submit paths (a
  submit failure marks the turn `interrupted` and no port event ever
  arrives). Limits: only daemon death inside a scheduled span leaves the
  lean compile until the next boot or memory change — degraded answers,
  never a leak; two processes refreshing mid-span race benignly.
- **Commands** (shared router: console, Telegram, Matrix):
  - `/remember <fact>` — explicit operator acceptance, appends to facts.md,
    recompiles, commits.
  - `/forget <id>` — removes the entry, recompiles, commits.
  - `/memory` — report: files, bytes, budget %, truncated flag.
  - `/memory review` — lists pending candidates with **full ids**.
  - `/memory accept <id>` / `/memory reject <id>` — candidate → stable
    memory (or rejected). `<id>` may be the full id or any unique prefix
    (`/cron rm` rule); an ambiguous prefix is refused, never guessed.
- **CLI** (`ka-agent memory …`): `path` prints the memory directory,
  `show [file]` prints one file, `edit [file]` opens `$EDITOR` on it and
  recompiles + git-commits the result (SEED.md §9.1 `ka-agent memory
  edit`). Files: facts (default) | profile | persona | preferences |
  environment | conventions.

## Memory candidates

- Stored in SQLite (`memory_candidates` table, schema v1): id, source,
  text, provenance, status (`pending`/`accepted`/`rejected`/`discarded`).
- **No conversational auto-promotion** (SEED.md §4.4): the only paths to
  stable memory are (1) explicit `/remember` from the operator and
  (2) acceptance of a candidate via `/memory accept` — or the web review
  inbox (`POST /memory/edit` stages, `POST /memory/candidate` decides), the
  same runtime paths as the console commands.
- **Review shows the exact delta** (B15a): `/memory review` renders each
  pending candidate as a bullet diff — target file, the `+ <text>` line,
  the entry it lands after — plus the dry-run compile impact
  (`compiled NB of BB`, and whether the budget would then truncate).
  The web review inbox rides the same accept path unchanged.
- **Accept is screen → apply → rollback** (B15a): accepting re-runs
  `screen_proposal` on the stored text (rules may have tightened since
  filing — a candidate that fails today's screen is refused), then
  snapshots the target file, applies, recompiles and commits; any failure
  (git missing, compile error) restores the snapshot byte-for-byte,
  recompiles and refuses — memory never lands half-applied.
- **Refusals at the `memory_propose` gate**: private conversations (row
  above); **scheduled sources** (cron/briefing turns cannot propose
  memory at all); and the three quota/warm-up gates (B15c) — the user's
  budget verdict is `Warn` (≥80% of a cap) or `Exceeded`, their newest
  `rate_limit` turn is less than 30 minutes old, or their live session
  has fewer than `[memory] extraction_min_turns` recorded turns (default
  5, `0` = off).
- **Removal/edit proposals are a NON-GOAL**: a poisoned removal is a
  §4-shaped deletion vector; `/forget` stays the only removal path.
- **Private conversations** (`/new private`) write nothing: `memory_propose`
  and `skill_propose` are refused for the session's turns, `/remember`,
  `notes_append` and every scheduling path (`/remind`, `/cron`, the
  `cron_manage` tool) answer with a refusal, and no episode rows are
  indexed — usage accounting (model, tokens, cost) still records, with
  no text. The conversation's engine state is contained too: its worker
  runs with a per-session scratch `KA_DATA_DIR` and `XDG_STATE_HOME`
  (`<root>/tmp/private-engine-<short-id>/`) so strands, waypoints and spilled
  tool outputs stay out of the shared engine dirs; the directory is
  destroyed when the conversation ends (and swept at boot). When a
  private conversation ends, its queued messages and approval cards
  (which embed tool arguments) are deleted from the store — with
  `secure_delete` on and a WAL checkpoint, so the text leaves the
  database file. One boundary stays: the in-memory event ring (256
  envelopes) may re-deliver a just-ended private conversation's last
  frames to a reconnecting surface until they age out — process memory
  only, never on disk.
- The model-side producer (`memory.propose` through the connector bridge)
  arrives with the MCP bridge in the next milestone.

## Episodic memory (history)

- **Storage**: SQLite FTS5 (`episodes_fts` virtual table, schema v1).
- **Indexing**: at every `idle` boundary (the prompt-cycle delivery point),
  the runtime writes two rows per turn: `(session, turn, "user",
  inbound_text)` and `(session, turn, "assistant", final_text)` where
  `final_text` is the concatenated outbox chunks.
- **Commands**:
  - `/history <query>` — FTS5 search, top 5 results with turn id + snippet.
    Leading-position filters (B17): `model:<id>`, `after:<YYYY-MM-DD>`
    (parsed in the profile timezone) and `status:<queued|submitted|
    running|done|error|aborted|interrupted|killed>` — applied as bound
    SQL predicates on the `turns` join, never as FTS MATCH syntax; a
    filter token mid-query stays ordinary searchable text. With a filter
    but no text (`/history status:error`), the command lists newest-first
    turns under the same predicates, the caller's own (every user's for
    the admin). Under a filter, episodes whose turn row was pruned drop
    out (the join turns inner) — accepted.

## What does NOT auto-update memory

Everything else. Conversational noise, tool results, error output — none of
it reaches stable memory without an operator act. This is the anti-pollution
guarantee (SEED.md §4.4).
