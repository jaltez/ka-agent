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
  - `/history show <turn_id>` — the stored exchange: `> {inbound}` +
    `{answer}` from the turn row + outbox chunks.

## What does NOT auto-update memory

Everything else. Conversational noise, tool results, error output — none of
it reaches stable memory without an operator act. This is the anti-pollution
guarantee (SEED.md §4.4).
