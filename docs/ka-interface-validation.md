# Ka interface validation — SEED.md evidence report

Status: 2026-10-15. This is the Phase 0 deliverable SEED.md §13 task 1 asks for: every gap G1–G20 with a verdict, runtime probe evidence, the gray zones settled, and the corrections applied to `SEED.md`.

Method and environment:

- Ka repository at `/home/javier/projects/ka`, commit `a56aae2d1b3a4c31c4910979e1033002eacc53a0` (workspace 0.3.0) — identical to the commit SEED.md was written against. No drift; the working tree was clean before and after (sha256 manifest compared, `target/` and `.git/` excluded).
- Probe binary: built from that commit with `cargo build --locked -p ka-harness --target-dir /tmp/ka-build` (20.3 s; `ka 0.3.0 (a56aae2d1b3a)`). `Cargo.lock` untouched (`--locked`).
- HTTP/SSE probes were driven by a throwaway Python client (`/tmp/ka-probe/probe.py`, stdlib only; not part of the repository). The commands below are the curl equivalents; outputs are the observed ones, abridged. Where a bullet shows no command, it is an SSE observation from the server session started with the command above it.
- Probes are keyless by intent: no model API key was passed or read by the harness. **Exception — disclosed in §7**: the first two `ka run` probes inherited the operator's own user config and key file and ran two real model turns (total cost ≈ $0.000622). Everything after that ran with an isolated `HOME`.

## 1. Gap verdicts (G1–G20)

| Gap | Verdict | Evidence |
|---|---|---|
| G1 serve/ACP run `Config::default()` (no model, free mode) | **confirmed** (source + runtime) | `crates/ka-harness/src/serve.rs:316-318`, `crates/ka-harness/src/acp.rs:117-120`, `crates/ka-engine/src/config.rs:676-679`; probes V-04 (canned echo), V-16 (`mode_changed: free`) |
| G2 no control routes over HTTP | **confirmed** (source + runtime) | `crates/ka-harness/src/serve.rs:241-277`; probe V-05 (all 404) |
| G3 asks never expire; a serve ask cannot be answered | **confirmed** (source) | `crates/ka-engine/src/voice.rs:2786-2823` (blocking loop, no timeout) + G2. Runtime wedge not reproducible keyless: asks require a model-driven tool call; needs the scripted-speaker spike |
| G4 mid-cycle prompts dropped silently (HTTP still 200) | **confirmed** (source + runtime) | `crates/ka-engine/src/voice.rs:2226-2232`, `:1835-1839`; probes V-06 and V-15-pre (bulk) |
| G5 serve ids are process-local counters | **confirmed** (source + runtime) | `crates/ka-harness/src/serve.rs:37-45`; probe V-08 (`s1` again after restart) |
| G6 ring "pinning" fails; backlog loses startup history | **confirmed** (source + runtime) | `crates/ka-harness/src/serve.rs:349-363` vs bootstrap order `crates/ka-engine/src/engine.rs:2454-2457` → `:2537`; probe V-15 |
| G7 no protocol version/handshake; closed enums | **confirmed** (source) | `crates/ka-protocol/src/lib.rs:557-560`, `research/ka/sdk.md:119-123` |
| G8 persistence per prompt cycle, no fsync/locking | **confirmed** (source) | `crates/ka-engine/src/engine.rs:2908-2917`, `:3246-3264`, `crates/ka-strand/src/lib.rs:590-604` |
| G9 interrupted turn = synthetic text + note only | **confirmed** (source) | `crates/ka-strand/src/lib.rs:509-526`, `crates/ka-engine/src/engine.rs:2393-2414` |
| G10 no scheduler/timers in Ka | **confirmed** (source) | `research/ka/roadmap.md:228`, `research/ka/agent-boundary.md:90-103` |
| G11 no long-term memory beyond strand/digest/MEMORY.md | **confirmed** (source) | `crates/ka-engine/src/hands/memory.rs:1-5`, `crates/ka-engine/src/conventions.rs:46-79` |
| G12 "always" is per tool name → project `.ka/ka.toml` | **confirmed** (source) | `crates/ka-engine/src/voice.rs:2697-2712`, `crates/ka-engine/src/config.rs:711-714` |
| G13 engine death silent (eprintln + closed channel) | **confirmed** (source) | `crates/ka-engine/src/engine.rs:129-136` |
| G14 steers never consumed on the live path | **confirmed** (source, exhaustive) | `steers` only pushed (`crates/ka-engine/src/engine.rs:1755`, `:2973`, `:3421`, `crates/ka-engine/src/voice.rs:1815`, `:2229`, `:3666`); sole drain is the canned path `crates/ka-engine/src/engine.rs:3450-3453`; the steer test asserts delivery only (`crates/ka-engine/src/hands/tasks.rs:626-653`) |
| G15 bash past 30 s detaches and survives | **confirmed** (source) | `crates/ka-engine/src/hands/jobs.rs:1-9`, `crates/ka-engine/src/config.rs:616-618` |
| G16 strand lookup/listing per cwd | **confirmed** (source) | `crates/ka-engine/src/engine.rs:145-163`, `crates/ka-strand/src/lib.rs:305-307` |
| G17 sandbox bash-only, off by default | **confirmed** (source) | `crates/ka-sandbox/src/lib.rs:1-22`, `:429-432` |
| G18 keys to plaintext `.env`; redaction output-only; unpriced cost 0 | **confirmed** (source) | `crates/ka-engine/src/config.rs:759-762`, `crates/ka-engine/src/hands/secrets.rs:1-3`, `README.md:306` |
| G19 ACP forwards only the first ask question; drops usage/title/idle | **confirmed** (source) | `crates/ka-harness/src/acp.rs:425-456`, `:468-484`; V-12 confirms ACP works keyless |
| G20 `ka run` auto-denies every ask | **confirmed** (source) | `crates/ka-harness/src/main.rs:953-963`. Runtime needs an ask, i.e. a model or scripted speaker — keyless-infeasible, left to the spike |

No gap was refuted or changed. The two claims SEED.md hedged as *(code-read inference)* (G6, G14) are now fully confirmed; G6 additionally has runtime proof.

## 2. Runtime probes

Binary: `/tmp/ka-build/debug/ka` (`ka 0.3.0 (a56aae2d1b3a)`), built as above. Server: `ka serve --addr 127.0.0.1:18417 --token probetok`, run in a scratch cwd with `KA_DATA_DIR` under `/tmp`. All processes were terminated afterwards; no listener remained on the probe port.

- **V-01 health.** `curl …/health` → `200 {"ok":true}`.
- **V-02 create.** `curl -X POST …/sessions` → `200 {"id":"s1"}`.
- **V-03 bootstrap order (fresh session), SSE.** Observed event order → `session_info, mode_changed, effort_changed, replay, inventory, todos` (+ `presence` on subscribe). The strand id arrives first, in `session_info`; `replay` is a single event; `inventory`/`todos` follow it. SEED §3.5's "read bootstrap events up to `replay`" is sufficient for strand-id capture; no SEED change needed.
- **V-04 first turn (G1).** `curl -X POST …/sessions/s1/prompt -d '{"text":"hello probe"}'` → `200 {"ok":true}`; SSE: `turn_started → delta ×3 → turn_finished{stop:"done", usage cost 0.0} → idle`, with text `"(ka, no model configured) heard: hello probe — set a model with --model or KA_MODEL to speak for real."` A fresh serve session has no model and costs nothing.
- **V-05 control routes (G2).** `POST /sessions/s1/{abort,answer,steer,compact}` and `DELETE /sessions/s1` → all `404 {"error":"not found"}`; unknown session → `404 {"error":"no such session"}`.
- **V-06 double prompt (G4).** Two prompts sent back-to-back (both HTTP 200). SSE result → only one turn ran: the stream shows the in-flight turn's events and a complete second reply for the first prompt, and **no events at all for the second**. The second prompt was dropped silently.
- **V-06b bulk variant (G4).** 90 prompts sent in a tight loop during the first G6 attempt → only ~2 completed turns (13 events total): every prompt that arrived mid-turn was dropped while its HTTP call still returned 200.
- **V-07 small reconnect.** `GET /events` with `Last-Event-ID: 0` after 3 turns → 19 backlog events including `replay` — reconnect mechanics work below the ring cap.
- **V-08 restart ids (G5).** Server killed and restarted; `POST /sessions` → `200 {"id":"s1"}` again. Ids are process-local.
- **V-09 `ka run --print ndjson`.** Exit 0; events begin → `session_info, model_changed, mode_changed, effort_changed, replay, inventory, todos, turn_started …`. **This probe accidentally used the operator's real model** (see §7) — which is precisely runtime proof that `ka run` applies the full user config chain (model, mode, effort) and its key files.
- **V-10 resume across restart.** A strand created by `ka run` (in the same cwd) was resumed via `POST /sessions {"resume":"<strand id>"}` → `200 {"id":"s2"}`; SSE bootstrap: `session_info, mode_changed, effort_changed, title, replay, inventory, todos, presence`, and the `replay` carried the prior exchange (user text, tool calls merged, thinking). **No `model_changed` appeared** although the strand was created with a configured model — because the initial config is never written to the strand (new finding F-1). Resume itself works from any surface.
- **V-11 exit codes.** `env -i PATH=$PATH HOME=<scratch> KA_DATA_DIR=<scratch> ka run --print text "answer yes"` → exit 0 with a final text answer.
- **V-14 resume on an empty store.** `env -i PATH=$PATH HOME=<scratch> KA_DATA_DIR=<empty> ka run --print text --session latest "x"` → exit 1, stderr `ka: no session matches 'latest'` (supports SEED §3.5's empty-session fallback).
- **V-13 keyless `ka run`, isolated HOME.** `env -i PATH=$PATH HOME=/tmp/ka-probe/fakehome KA_DATA_DIR=… ka run --print text "keyless check"` → exit 0, output `(ka, no model configured) heard: keyless check — …`. With the user config out of reach, the canned path is exactly as documented.
- **V-15 ring eviction (G6).** 95 turns run sequentially (waiting for `idle` between prompts, so every turn completed; `max_seq` ≈ 570 > `RING_CAP` 512). Reconnect with `Last-Event-ID: 0` → **321 backlog events with no `replay` and no `session_info`**: the "pinned" startup replay had been evicted, despite `crates/ka-harness/src/serve.rs:37-40` claiming it never is. No serve test covers this path (tests use a handful of events).
- **V-16 fresh-session mode (G1).** Bootstrap payloads → `mode_changed: "free"`, `effort_changed: "off"` — a fresh serve/ACP engine is in free mode, as claimed.

## 3. SEED.md claim verification

Two read-only passes checked every SEED.md statement that asserts something about Ka (lines 70–235 and 236–660), plus the Ka-related claims in §7–§13 verified when writing the document and re-checked by the citation runner.

- **74 claims checked; 72 `supported`; 2 `wrong-citation`** (claims true, evidence cell too narrow):
  - **C-1** §3.1.2 "Extension" row cited only the `Hook` struct for five mechanisms → evidence split across MCP, skills, subagents, features and hooks (SEED fixed).
  - **C-2** §3.1.1 "Tools" row did not cite the `Clearance` enum → added `crates/ka-engine/src/hands/mod.rs:38-44` (SEED fixed).
- **C-3 (wrong-claim, nuanced, SEED fixed):** "restores model, mode and effort from the strand" holds **only when the strand contains `Change` records** — appended for mid-session `set_model`/`set_mode`/`set_effort` and overflow promotion (`crates/ka-engine/src/engine.rs:1639`, `:1661`, `:1676`, `:2930`), never for the initial config. Observed in V-10. Consequence now stated in SEED §3.6: the worker must always send `set_mode`/`set_model` after attach.
- **Hermes/OpenClaw pages (class D):** all five cited pages re-read; every §1.4 capability claim still holds (memory + skills review gate + channels + scheduling + security layers for Hermes; gateway/marketplace/cron/loopback+pairing/one-trust-boundary for OpenClaw). No source code or prompts of either was read.

### Internal-consistency findings (class F)

- **F-1** Initial config is not persisted to strands → SEED §3.1.1 and §3.6 updated (see above).
- **F-2** `XDG_CONFIG_HOME` is honored by Ka's user config layer (`crates/ka-engine/src/config.rs:789-793`) → HOME-only isolation is insufficient for `ka.toml`. SEED §3.1.5 and §6.4 now require pinning/unsetting it (waypoints confirmed under `KA_DATA_DIR`, `crates/ka-engine/src/engine.rs:3323-3325`; `XDG_CACHE_HOME` feeds the sandbox allowlist, `crates/ka-sandbox/src/lib.rs:334`).
- **F-3** `MEMORY.md` is re-read from disk **per prompt** (`crates/ka-engine/src/voice.rs:1634-1639`, called from `crates/ka-engine/src/engine.rs:2879-2889`) → SEED §4.4's open question closed: memory updates apply on the next prompt, no worker restart.
- **F-4** `[[rules]]` match MCP hands by exact `<server>.<tool>` name and are evaluated before mode logic (`crates/ka-engine/src/mcp.rs:842-861`, `crates/ka-engine/src/voice.rs:3010-3038`) → SEED §6.3's `kagent.<tool>` allow-rule plan works; tag upgraded from [Provisional] to [Requirement].
- **F-5** Telegram data confirmed (see §4); SEED §5.4 updated with the official framing (the ~30/s figure is a bulk-broadcast cap; `editMessageText` has no documented per-second cap).
- **F-6** G6 and G14 upgraded from *(code-read inference)* to confirmed (SEED §3.1.4).
- **F-7** SEED §10 Phase 0 and §13 task 1 updated to reflect what this report already answers; only cgroup kill semantics remain open.
- **F-8** Consistency sweep of §9.1 vs §10 Phase 1 deliverables, cross-references and terminology: no further conflicts found after the 2026-10-15 corrections.

## 4. Telegram Bot API data (class C)

| Fact | SEED said | Official says | Source |
|---|---|---|---|
| Max text message | 4096 chars | "1-4096 characters after entities parsing" | https://core.telegram.org/bots/api/methods/sendmessage |
| `callback_data` | 64 bytes | "1-64 bytes" (on `InlineKeyboardButton`) | https://core.telegram.org/bots/api/types/inlinekeyboardbutton |
| Bot download via `getFile` | 20 MB | "up to 20MB in size"; link valid ≥ 1 h | https://core.telegram.org/bots/api/methods/getfile |
| Bot upload (`sendDocument`) | 50 MB | "up to 50 MB … may be changed" | https://core.telegram.org/bots/api/methods/senddocument |
| Long polling + offset | persisted offset after commit | `timeout` param; "update is considered confirmed as soon as getUpdates is called with an offset higher than its update_id"; recalc offset after each response | https://core.telegram.org/bots/api/methods/getupdates |
| Rate limits | ~30/s global, ~1/s per chat | per chat ~1 msg/s (bursts tolerated, then 429); groups 20/min; ~30/s framed as a **bulk-broadcast** cap (paid broadcasts up to 1000/s) | https://core.telegram.org/bots/faq |
| Typing action | ~5 s | "set for 5 seconds or less" | https://core.telegram.org/bots/api/methods/sendchataction |
| `editMessageText` pacing | 1 edit / 3 s | no per-second cap documented; 1-4096 chars; some messages editable only 48 h | https://core.telegram.org/bots/api/methods/editmessagetext |

All SEED values hold; two nuances recorded (rate-limit framing; edit-rate undocumented → SEED now states its 3 s pacing is our own policy, with 429 backoff).

## 5. Gray zones

| Question | Answer | Evidence |
|---|---|---|
| Serve ring evicts startup replay? (was inference) | Yes — runtime-confirmed | V-15; `crates/ka-harness/src/serve.rs:349-363` |
| Steers consumed on live path? (was inference) | No — exhaustive source-confirmed | §1 G14 rows |
| When is `MEMORY.md` read? | Every prompt, from disk | `crates/ka-engine/src/voice.rs:1634-1639` |
| Do `[[rules]]` cover MCP tools? | Yes, exact `<server>.<tool>` match, before mode | `crates/ka-engine/src/mcp.rs:842-861`, `crates/ka-engine/src/voice.rs:3010-3038` |
| Does HOME isolate everything? | No — `XDG_CONFIG_HOME` bypasses it for `ka.toml`; pin it | `crates/ka-engine/src/config.rs:789-793` |
| Do `KA_DATA_DIR`/`XDG_STATE_HOME` isolate strands/spills/trust/jobs? | Yes (waypoints under `KA_DATA_DIR`) | `crates/ka-strand/src/lib.rs:244-253`, `crates/ka-engine/src/hands/mod.rs:395-398`, `crates/ka-engine/src/trust.rs:23-27`, `crates/ka-engine/src/hands/jobs.rs:64-68` |
| Cgroup kill semantics for workers | **Requires spike** (systemd transient scope; needs implementation code) | — |

## 6. Changes applied to SEED.md

1. §3.1.1 Sessions/strands row: `Change`-record qualifier + probe V-10 (F-1).
2. §3.1.1 Tools row: added `Clearance` enum citation.
3. §3.1.2 Extension row: evidence split (MCP, skills, subagents, features, hooks).
4. §3.1.4 G6 and G14: inferences upgraded to confirmed, evidence extended.
5. §3.1.5 isolation bullet: `XDG_CONFIG_HOME` requirement (F-2).
6. §3.6 steps 2–3: resume restores only recorded `Change`s; `set_mode`/`set_model` re-assert now a hard requirement (F-1).
7. §4.4: `MEMORY.md` read timing settled (F-3).
8. §5.4: Telegram limits marked verified with official framing; `editMessageText` pacing note (F-5).
9. §6.3: rules-matching verified; tag [Provisional] → [Requirement] (F-4).
10. §6.4: worker env adds pinned `XDG_CONFIG_HOME`; waypoints location noted (F-2).
11. §10 Phase 0 deliverables and §13 task 1: answered questions point here; remaining work named (F-7).

No other SEED.md content changed; the structure checker still passes (§8).

## 7. Incident: unintended model usage during probes

The first two `ka run` probes (V-09, and the first half of V-11) ran with the ambient environment: Ka's config chain read the operator's `~/.config/ka/ka.toml` (`model = "zai/glm-5.3-flash"`, `mode = "free"`) and authenticated with `~/.config/ka/.env`. Two real model turns ran; the strand usage records show `cost 0.000413975` and `cost 0.0002079` — **total ≈ $0.000622**. The harness never read, printed or passed any key; Ka used its own user files, exactly as designed for interactive use.

Response: all subsequent runs used `env -i HOME=<scratch> KA_DATA_DIR=<scratch>` (V-13, V-14, V-15), which reproduces the canned, zero-cost path. Design impact: this is runtime proof of the ambient-config hazard SEED §3.4/§3.1.5 already warns about — any Ka Agent worker **must** control `HOME`/`XDG_CONFIG_HOME`/`KA_DATA_DIR` or it will silently inherit the operator's model, mode and keys.

## 8. Final checks

- SEED.md structure/citation checker (same rules as the seed goal): **0 FAIL**; all Ka citations exist with in-range lines.
- Ka repository: HEAD, `git status --porcelain` and the 25 067-file sha256 manifest identical before and after.
- No probe processes or listeners remain (`ss -ltn` on 18417 empty; no `ka serve` processes).
- Still requiring implementation code (carried into Phase 0): cgroup kill semantics for workers; runtime timing of an unanswered ask and of G20's auto-deny (both need `spawn_with_speaker`).
