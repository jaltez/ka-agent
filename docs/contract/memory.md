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
  Bullets whose `valid_until` has passed are excluded from the current
  compile; the Markdown and git history keep them, and an `as_of` search
  can still read them by date. Files that don't fit are truncated
  entry-by-entry; later files are replaced by a one-line omission marker.
  The result is written to `<engine-home>/.config/ka/MEMORY.md` (ka's
  user-tier memory, re-read per prompt — validation §5: no worker restart
  needed).
  An entry's `valid_until` is set when it is accepted (a dream's
  `VALID_UNTIL`, below) or later by `/memory expire`; it must name a real
  `YYYY-MM-DD` day, and the entry stays valid through that day.
- **Automation variant** (B15b): turns a schedule started (`cron`,
  `cron:<id>`, `briefing`, `trigger:sched:<id>` sources) run against an `Automation`-scope
  compile that keeps only the machine facts (`environment.md` +
  `conventions.md`) — profile, preferences, projects and facts are
  dropped, so crons neither read nor feed on personal memory. The daemon
  installs it right before the scheduled submit and restores the full
  compile at every exit of that bracket: the cycle's terminal points
  (`turn_finished` before any drain, `idle` before any drain,
  engine-closed) and the Err arms of both bracketed submit paths (a
  submit failure marks the turn `interrupted` and no port event ever
  arrives). A schedule-trigger fire publishes the same per-user turn
  context as a chat turn, so the bridge gates read the trigger source.
  Limits: only daemon death inside a scheduled span leaves the lean
  compile until the next boot or memory change — degraded answers, never a
  leak; two processes refreshing mid-span race benignly.
- **Webhook memory scope**: a webhook trigger's payload carries
  `memory_scope` — `automation` (the default; absent in every payload
  written before the scope existed) or `creator`. An `automation` fire
  runs inside the schedule-trigger bracket: the automation compile goes in
  before the submit and the full compile comes back when the fire settles,
  on success or failure. `creator` is the creator's explicit opt-in: the
  fire keeps the full compile. A schedule trigger always runs
  `automation`, whatever its payload says. Every turn — a fire, a cron, a
  chat message — publishes its scope as the per-user turn-context field
  `memory_scope` (`automation`/`creator`), so the bridge gates read it and
  a later turn never inherits an earlier one's. An `automation` fire also
  gets no recall block and contributes nothing to dream input.
  Limit: the turn context is per user, so a trigger fire that overlaps its
  creator's own live turn can read that turn's context. An `automation`
  fire therefore also writes a per-trigger marker
  (`memory.automation.active.<user>.<task>`); any fresh marker makes every
  memory tool for that user fail closed while the fire runs. The marker is
  cleared at the fire tail and ignored after one hour, so a crash cannot
  block memory forever. Operators set the webhook scope with
  `ka-agent trigger scope <id> automation|creator`; schedule triggers
  refuse the command because they always run automation.
- **Commands** (shared router: console, Telegram, Matrix):
  - `/remember <fact>` — explicit operator acceptance, appends to facts.md,
    recompiles, commits.
  - `/forget <id>` — removes the entry, recompiles, commits.
  - `/memory expire <id> <YYYY-MM-DD | clear>` — set, change or lift the
    validity of one accepted entry: its provenance comment is edited in
    place (the bullet, source and accept date stay), then recompile and
    commit (`memory: expire <id> <date>`). The entry leaves the current
    compile the day after its last valid day; the Markdown and an `as_of`
    search keep it. A date that is not a real day is refused before
    anything is written.
  - `/memory` — report: files, bytes, budget %, truncated flag; a listed
    entry shows `· until <date>` when it has an expiry.
  - `/memory search <query>` — accepted-memory and past-turn search
    (phase A; hybrid when embeddings are configured). Refused in a
    private conversation.
  - `/memory index [status|rebuild]` — FTS/vector counts, model,
    dimension, staleness, and a rebuild (embeddings optional).
  - `/memory dream` — one review-gated extraction pass (disabled by
    default; see "Dream extraction").
  - `/memory review` — lists pending candidates with **full ids**, and
    the accepted entries a candidate supersedes, conflicts with or
    overlaps.
  - `/memory accept <id>` / `/memory reject <id>` — candidate → stable
    memory (or rejected). `<id>` may be the full id or any unique prefix
    (`/cron rm` rule); an ambiguous prefix is refused, never guessed.
    `/memory accept <id> supersede <old>` adds the candidate and removes
    the named stable entry in one rollback-protected step; git history
    keeps the old text. Removal stays operator-only.
  - `/memory skill <id>` — turns one accepted procedural memory into a
    skill candidate (see "Memory to skill"); nothing is installed until
    `/skills accept`.
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
  The web review inbox rides the same accept path unchanged. A dream
  candidate's links to accepted entries — `supersedes [id]`, `conflicts
  with [id]`, `overlaps [id]`, each with the entry's bullet — follow the
  provenance line.
- **Accept is screen → apply → rollback** (B15a): accepting re-runs
  `screen_proposal` on the stored text (rules may have tightened since
  filing — a candidate that fails today's screen is refused), then
  snapshots the target file, applies, recompiles and commits; any failure
  (git missing, compile error) restores the snapshot byte-for-byte,
  recompiles and refuses — memory never lands half-applied. A
  `valid_until:<date>` note in the provenance (a dream's `VALID_UNTIL`)
  lands with the bullet; one that is not a real `YYYY-MM-DD` day refuses
  the acceptance.
- **Refusals at the `memory_propose` gate**: private conversations (row
  above); **scheduled sources** (cron, briefing, and schedule-trigger turns cannot propose
  memory at all); **the `automation` memory scope** (a webhook fire under
  its default scope, for the same reason); and the three quota/warm-up
  gates (B15c) — the user's
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

## Search (`memory_fts`)

- **Derived state, not the source of truth**: `memory_fts` (schema v20) is
  one FTS5 row per accepted bullet. The Markdown tree plus git remains the
  source. Rows: `id`, `username`, `file`, `text`, `keywords`, `source`,
  `accepted_ms`, `valid_until`.
- **Refresh**: rebuilt after every accepted change (`/remember`, `/forget`,
  candidate accept, `/memory undo`) and lazily before a search when the
  entry fingerprint differs. An external `ka-agent memory edit` is picked
  up on the next search: the FTS mirror is rebuilt and the vector cache is
  cleared and re-embedded in the background. `ka-agent user purge` deletes
  the user's rows after removing the tree.
- **Keywords**: `ka_agent_memory::keywords` takes up to eight distinct
  lowercased tokens of three or more characters, minus common function
  words. Deterministic, no model call. The FTS match searches the text and
  the keywords.
- **Query semantics**: `memory_fts` uses an OR of quoted prefixes ranked by
  BM25. This is recall-oriented, unlike `/history`'s AND-of-terms. A
  missing word does not hide a relevant bullet, and `deploy` matches
  `deploys`. The current view excludes bullets whose `valid_until` has
  passed and bullets accepted after the as-of date. `as_of` reads the
  bullets that were valid on that date.
- **Surfaces**: `/memory search <query>` (shared router) and
  `kagent__memory_search` (read-only MCP tool). Both return accepted
  memory first, then the caller's past turns, with `[mem:<id>]` or
  `[turn:<id>]`, a date, and the file/source or role. Retrieved memory is
  labeled data, not instructions. `memory_search` also accepts
  `session:<id-or-prefix>`, `after:YYYY-MM-DD`, `before:YYYY-MM-DD`,
  `as_of:YYYY-MM-DD`, and `expand:0..2`; `expand` returns the neighboring
  turns of an episode hit. Knowledge-base documents stay on `kb_search`.
- **Gates**: `/memory search` and `memory_search` refuse in a private
  conversation. `memory_search` and `memory_propose` also refuse a turn
  whose turn context carries `memory_scope = automation` — a webhook fire
  under its default scope — and, as before, a scheduled-source turn
  (`cron`, `cron:<id>`, `briefing`, `trigger:sched:<id>`). B15b's
  automation compile already keeps those turns from reading personal
  memory; the gates close the tool paths too. A context without the field
  predates it and is not automation.
- **Eval**: `cargo test -p ka-agent --test memory_eval -- --nocapture`.
  The corpus deliberately exceeds the 6 KB compile budget, so the
  compiled-`MEMORY.md` baseline misses truncated bullets while FTS finds
  them. It prints Recall@5 per class and p50 latency; phase A asserts the
  lexical gain and no class regression. The next-round test covers
  expired-current versus as-of, state recovery from the artifact, a
  retrieval-level task proxy, and p50/p95 latency. A real embedding model
  is opt-in through `KA_MEMORY_EVAL_EMBEDDING_MODEL` and `--ignored`.

## Recall and embeddings (B1)

- **Defaults**: `memory.recall.enabled = true` (FTS-only until an
  embedder is configured) and `memory.embeddings.provider = "none"`.
  The operator can disable recall or enable a backend.
- **Hybrid retrieval**: `search_memory_and_episodes` runs the memory FTS
  list, the optional vector list, and the episode FTS list, then fuses
  them with reciprocal rank fusion (k = 60). `/memory search` and
  `kagent__memory_search` use the same path. A vector backend or index
  error degrades to FTS; it never fails the turn.
- **Vector index**: `memory_vectors` (schema v21) stores one unit-length
  f32 vector per accepted bullet, per user, with the model id and
  dimension. The runtime rebuilds it from the Markdown source when the
  entry fingerprint or model changes. A full per-user exact cosine scan
  is used; the cache is loaded on first query. The per-user cap is 50k
  vectors (about 200 MB at 1024 dimensions), after which only the newest
  entries are indexed. `memory_index_meta` records model, dimension,
  fingerprint, and timestamp.
- **Embedding backends**: `openai`/`ollama` post to an OpenAI-compatible
  `/v1/embeddings` endpoint (`base_url`, `model`, optional
  `api_key = "secret:<name>"`). `local` uses fastembed behind the
  `local-embeddings` build feature; the model is downloaded on first use
  under `<data>/models`. `provider = "none"` is the default and keeps
  the binary lean.
- **Qwen3 embeddings**: a `local-qwen3` build adds `model =
  "qwen3-embedding-0.6b"` to `provider = "local"`. It downloads
  `Qwen/Qwen3-Embedding-0.6B-GGUF` (Q8_0, about 639 MB) and the tokenizer
  from `Qwen/Qwen3-Embedding-0.6B` into `<data>/models` on first use, and
  loads them through the safe `from_gguf` path. Vectors are 1024 wide.
  Recall queries carry the model card's `Instruct:` prefix; documents do
  not.
- **Reranker**: `[memory.rerank] enabled = false` by default, `model`
  defaults to `BAAI/bge-reranker-v2-m3` (about 2.3 GB on first use). A
  `local-embeddings` build exposes `runtime::memory_rerank::build_reranker`.
  Only the explicit `memory_search` tool reranks its fused top-k; the
  per-turn recall block does not. A disabled or failing reranker keeps the
  fused order.
- **Recall block**: interactive turns with `[memory.recall] enabled`
  get a `<recalled_memory source="ka-agent">` prefix. The block is
  capped at `memory.recall.budget_tokens` (600) and the combined memory
  context is capped at `memory.max_context_tokens` (2000), measured with
  a five-bytes-per-token estimate for the compiled `MEMORY.md`. It is
  skipped in private conversations, for `cron`/`cron:<id>`/`briefing`/
  `trigger:sched:<id>` sources, and for a webhook fire under the
  `automation` memory scope.
  The text is escaped and labeled data, not instructions.
- **Status/rebuild**: `/memory index status` reports the FTS and vector
  counts, model, dimension, recall state, and staleness.
  `/memory index rebuild` recompiles the FTS mirror and, when an
  embedder is configured, re-embeds the accepted bullets. `ka-agent user
  purge` deletes both derived tables with the memory tree.
- **Eval**: `cargo test -p ka-agent --test memory_eval -- --nocapture`.
  The B1 fixture embedder is a deterministic concept table, not a real
  model. It proves the vector channel and RRF fusion on the phase B
  corpus (paraphrase and multilingual gains, no lexical regression, p50
  under 50 ms). Real model quality is a separate manual check.

## Dream extraction (B2)

- **Config**: `[memory.dream] enabled` (default `false`), `idle_hours`
  (3), `min_turns` (10), `max_candidates` (5), `timeout_secs` (120).
  `/memory dream` runs one pass when enabled; automatic idle scheduling is
  not wired yet, so the command is the trigger.
- **Eligibility**: the newest non-private, non-archived session of the
  caller, idle for at least `idle_hours`, with at least `min_turns`
  eligible turns. A processed session is remembered in
  `memory.dream.<user>.<session>`; new turns make it eligible again.
- **Input**: the daemon assembles up to 8 KB of user messages and
  assistant final text from the session's turns. Tool output is never
  included. Scheduled-source turns are excluded through `turn_origins`
  (schema v22), written on every turn insert, plus the existing
  `cron`/`cron:<id>`/`briefing`/`trigger:sched:<id>` source check. A
  webhook trigger carries `trigger:<id>` and is not a scheduled turn, but
  its turns are left out the same way unless the trigger's memory scope is
  `creator`: a fire under the default `automation` scope is an automation
  turn for the dream too. An origin whose trigger no longer resolves
  counts as automation.
- **Child**: a disposable engine worker on a fresh strand in `readonly`
  mode with an empty `allowed_tools` list (no tools). It sees the compiled
  memory (under the existing 6 KB budget) and the excerpt, not the store.
  A child ask is denied fail-closed and audited; the child dies with the
  pass. A `runs` row (`kind = memory_dream`) and an audit row
  (`memory.dream`) record the pass.
- **Output**: lines of the form
  `FACT: <text> | FILE: facts|preferences|environment|profile |
  SUPERSEDES: <exact existing bullet or -> | VALID_UNTIL: <YYYY-MM-DD or ->`.
  The last three fields are optional. The daemon screens every fact with
  `screen_proposal`, and refuses a line that is over 300 chars, names an
  unknown file, or carries a `VALID_UNTIL` that is not a real
  `YYYY-MM-DD` day. A valid date is stored in the candidate's provenance
  as `valid_until:<date>` and lands with the bullet when the candidate is
  accepted. Candidates are staged only; nothing is auto-accepted.
- **Consolidation**: each fact is compared with the accepted entries and
  the caller's pending candidates by keyword Jaccard. A score of `>= 0.8`
  with the same values is a duplicate and is dropped. The same words
  around a *changed value* — a number, date, version, host, path or id
  that each side names and the other does not — is not a duplicate:
  `Laptop has 64 GB` is a change when memory says `32 GB`. For what is
  staged, the closest accepted entry at or above `0.5` is noted in the
  provenance: a resolved `SUPERSEDES` wins (`supersedes:<id>`); otherwise
  a changed value gives `conflicts:<id>`, and shared keywords without one
  give `overlaps:<id>`. Notes only — the operator decides.
- **Review**: `/memory review` shows the provenance line and, for each
  note, the accepted entry's id and bullet, so an operator can copy the
  old id into `/memory accept <id> supersede <old>` — for a conflict that
  is the replace; a plain accept keeps both bullets.
- **Zero auto-apply** is the contract: dream can only call
  `add_candidate_for_user`; it cannot accept, forget, or edit stable
  memory. Removal and supersession require the operator.

## Memory to skill

`/memory skill <id>` turns one accepted **procedural** memory into a
skill *candidate*. A procedure is a bullet with a clause that opens on an
action verb (`run …`, `to deploy, …`, `para desplegar, …`), a `how to`, a
step connector (`then`, `luego`) or a command chain (`&&`, a ` --flag`);
a plain fact (`lives in Madrid`) is refused. No model call is made:

- the name is the bullet's first keywords joined by dashes
  (`deploy-release-train`), within the skill-name rule (`[a-z0-9-]`, at
  most 48 characters);
- the description is the bullet's words cut at a word boundary under 120
  characters, reduced to letters, digits and spaces — the skill accept
  path writes it unquoted into TOML frontmatter, so nothing TOML-special
  may survive;
- the body is the bullet itself, followed by a comment naming the memory
  it came from (`<!-- from memory mem_… -->`), under the same invisible
  and credential screens and the 8 KiB cap as `skill_propose`.

The candidate joins the `skill_propose` queue — `/skills review|accept|
reject` — so it is operator-gated: `/memory skill` installs and enables
nothing. A name that a library skill or a pending proposal already has is
refused, not suffixed.

## What does NOT auto-update memory

Everything else. Conversational noise, tool results, error output — none of
it reaches stable memory without an operator act. This is the anti-pollution
guarantee (SEED.md §4.4).
