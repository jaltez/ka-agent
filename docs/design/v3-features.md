# v3 feature catalog — 2026-11-04

Baseline: v0.13.0 (the v2 audit's output), `main` @ `caa0a61`. Inventory at
baseline: 29 chat commands, 42 CLI commands, 46 config keys, 21 web routes.

## Method

Two vygr research runs plus six targeted first-hand extracts (artifacts under
`/tmp/vygr-runs/`): R1 asked which features users of comparable agents consider
distinctive or acclaimed (`r1-answer.md`), R2 asked what they complain about,
with a disconfirmation pass (`r2-answer.md`); the extracts fill R1's admitted
gap on the assistant-class products — the r/openclaw "87 use cases" thread
(`e-reddit87.txt`), two founder comparisons (`e-vs.txt`, `e-vs2.txt`), the
WhatsApp integration guide (`e-wa.txt`), OpenClaw issue #18424 (`s5.json`) and
Hermes issue #13065 (`s6.json`). Boundary rule from `docs/contract/ka-port.md`:
everything above `KaPort` (surfaces, policy, memory, scheduling, integrations,
ops) is ka-agent work; a feature whose essence lives below the port — model
wires/dialects, strand storage/replay, context window + compaction, tool
*execution* hands, sandbox mechanics, engine-side retry/fallback internals —
goes to `docs/handoff/ka-harness-features.md` (an uncommitted handoff, not a
commitment), never this catalog. Scoring as in v2: priority = impact (1–3) ×
frequency (1–3), bands P0 ≥ 6, P1 3–5, P2 ≤ 2; effort S < 1 day, M = days,
L ≥ week; risk 2 = touches approvals/security/engine pin/protocol submission.
Every Research basis cites its source URL; every Adaptation names files and
symbols verified at `caa0a61`. This document is a catalog only — nothing here
is implemented.

## Top 10 (+1)

| # | title | priority | effort | impact |
|---|---|---|---|---|
| 1 | Event-driven watchers (`/watch`) | P0 | M | zero-token checkers turn "is it done yet?" polling into signal-only notices — the best-evidenced community workaround in the corpus |
| 2 | Vision inbound: photo → model image | P1 | M | photos become first-class input on vision-capable models; no auxiliary-vision-model detour |
| 3 | Context transparency | P1 | S | degradation is visible before quality drops, and the next turn's cost is known before it is spent |
| 4 | Voice out (TTS replies) | P1 | M | voice-first operators get spoken replies through the existing local-provider plumbing |
| 5 | `/history summarize` | P1 | S | months of history answerable in one turn without polluting stable memory |
| 6 | Memory distillation preset | P2 | M | durable facts distill themselves weekly, still operator-gated through cards |
| 7 | `/file <path>` — push a file to the phone | P2 | S | one command moves any readable workspace file to any surface |
| 8 | TUI diff pager on approval cards | P2 | S | the full diff is reviewable at the exact moment it is approved |
| 9 | WhatsApp channel (wacli) | P2 | L | the personal-messaging default joins Telegram + Matrix |
| 10 | Skill usage stats + proposal nudge | P2 | S | proposals happen when they should; usage counts show which skills pay off |
| 11 | Doctor zero-telemetry line | P2 | S | "what leaves this machine" answered in one line, as a trust differentiator |

## Features

### 1 Event-driven watchers (`/watch`)
- Research basis: R2's best-evidenced cluster. OpenClaw's in-conversation heartbeat either interrupts the chat or fails silently — "Silence in automation is dangerous because you assume everything is fine while messages are not being delivered" (https://www.reddit.com/r/AISEOInsider/comments/1rirmw7/openclaw_heartbeat_dm_broke_and_nobody_noticed, https://docs.openclaw.ai/gateway/heartbeat) — and the community workaround is event-driven zero-token checkers: a bounded script that exits silently when nothing changed and only delivers/wakes on a real event, with `silent_if_empty` (https://github.com/Kuberwastaken/agent-notifications). ka-agent's isolated crons already fix the in-conversation half; the zero-token checker half is missing.
- Adaptation: `/watch add every 5m <name> -- <command>` creates a cron whose spec text is the command and whose mode is `CronMode::NoAgent("shell")` — a new kind beside `"calendar"` (`crates/ka-agent-core/src/task.rs:130`, calendar reference at `:370`), dispatched through the registered no-agent handler (`crates/ka-agent-runtime/src/runtime/scheduler.rs:393`, `set_no_agent_handler` at `:624`). The handler runs the command with `run_gate`'s exact bounded-exec shape (`sh -c`, workspace cwd, stdin null — `scheduler.rs:654`): exit 0 with empty stdout = silent (re-arm, zero tokens, the `GateOutcome::Skip` shape); non-empty stdout = `⏰ watch <name>: <first lines>` Notice, no model; non-zero exit = the notice plus a failed run row. `--wake` creates a gated Agent cron instead — the existing `--if` machinery already wakes the agent with `<gate-output>` appended and skips silently when empty (`split_gate`, `crates/ka-agent-channels/src/commands/schedule.rs:96`; `spec.gate`, `task.rs:209-213`; `scheduler.rs:316-371`). `/watch [list|rm|pause|resume]` mirrors `/cron` (`schedule.rs:220`). Grants and sandbox rules apply as for any exec. Contract: extend `docs/contract/cron-and-refs.md` with the `shell` no-agent kind.
- Acceptance: a watcher on a printing command emits the `⏰` notice and records a run; on `exit 0` with no output the occurrence re-arms with no event, no token, no run row; `/watch list` shows name + next fire like `/cron list`.
- Priority: P0 · effort M · risk 1

### 2 Vision inbound: photo → model image
- Research basis: Hermes #13065 — routing every image through an auxiliary vision model adds latency, cost, and information loss ("text description ≠ seeing the image"); users on vision-capable main models want native image input (https://github.com/NousResearch/hermes-agent/issues/13065). First-hand: the 87-use-cases operator diagnoses a Termux bug by sending a screenshot to the agent (https://www.reddit.com/r/openclaw/comments/1sjr3ep/my_87_use_cases_for_openclaw_they_became_more/).
- Adaptation: in the Telegram photo arm (`crates/ka-agent-channels/src/telegram/channel.rs:490-520` — today: save, ack, caption→turn) attach the image bytes to the turn via `Submit.images`, which already exists in the worker protocol (`docs/contract/worker-protocol.md:24,58`; `ka_agent_port`'s `submit(text, images, skills)` at `crates/ka-agent-port/src/port/session.rs:216-233`), in addition to today's attachment save; the caption stays the text. Documents with an image mime and Matrix inbound media take the same path (`matrix/channel.rs:154` → `save_media` at `:223`). Capability gate: the merged catalog row carries an `input: Vec<Modality>` (Text/Image) list, read through `ka_agent_port::catalog` exactly as the models.dev merge populates those same rows; when the configured model is not marked image-capable, keep today's file-reference behavior and append the hint `this model can't see images — the file is saved as @attachments/…` to the ack. Native routing when the model *is* vision-capable is engine work — handoff item 1; ka-agent starts sending images regardless.
- Acceptance: telegram e2e: a captioned photo under a vision-marked model submits a turn whose `images` array is non-empty with the caption as text; under a text-only model the ack carries the can't-see hint and `images` stays empty; the file still lands in `attachments/` in both cases.
- Priority: P1 · effort M · risk 2

### 3 Context transparency
- Research basis: R2's "opacity, not context size" finding — the top-voted framing: "Does the user know when the output degrades due to context limits?" (https://www.reddit.com/r/opencodeCLI/comments/1spztym/does_anyone_else_lose_trust_in_long_opencode), and resuming a 900k-token session should show the cost rather than "silently lobotomizing the model" (https://news.ycombinator.com/item?id=47878905). Compaction is shipped engine-side; the telemetry, degradation signal, and cost preview are not.
- Adaptation: (a) count compactions per session — the `PortEvent::Compacting { done }` arm (`crates/ka-agent-runtime/src/runtime/events.rs:372`) increments a counter in `Shared`, surfaced as `compactions: u32` on `StatusReport` (`crates/ka-agent-runtime/src/status.rs:7`). (b) render it — the TUI status line already prints `ctx {pct}%` (`apps/ka-agent/src/term/view.rs:106`); extend to `ctx 82% · compacted 2×` and, at ≥ 80%, append `· nearly full — /new or /compact; answers may degrade`; `/status` shows the same. (c) cost preview — `/status` computes the projected input cost of the next turn from the reported used-context tokens × the catalog row's `price.input_per_mtok`, read via `ka_agent_port::catalog` and displayed only when the row's `priced` flag is set (placeholder prices must not display, per that field's own contract).
- Acceptance: after two engine compactions in one session, `/status --json` reports `"compactions": 2` and the TUI status line shows `compacted 2×`; at ≥ 80% context the line carries the `/new or /compact` hint; `/status` under a priced row shows a `~$X.XX/turn at <model>` figure and under an unpriced row shows none.
- Priority: P1 · effort S · risk 1

### 4 Voice out (TTS replies)
- Research basis: OpenClaw #18424 requests voice-in *and* voice-out — today "replies are text only", which "makes voice-first use awkward", and cloud STT/TTS is rejected on recurring cost and privacy grounds; the ask is local TTS (https://github.com/openclaw/openclaw/issues/18424).
- Adaptation: `[voice]` already carries `provider`/`base_url`/`model` for transcription (`crates/ka-agent-core/src/config_keys.rs:390-406`); add `voice.out` (bool, default `false`, `Apply::Restart`). Sibling TTS function in `crates/ka-agent-channels/src/voice.rs` beside `transcribe` (`:48`): OpenAI-compatible `POST {base}/audio/speech` returning audio bytes. When on, the Telegram surface sends the settled answer as a `sendVoice` (multipart, the existing `AgentEvent::File` delivery path — `telegram/surface.rs:594` and `deliver_file`), falling back to today's text message with one Notice on first failure (the "a refusal never disables the channel" policy). Matrix uploads the audio as `m.audio` via its existing upload path (`matrix/client.rs:228-241`). Toggle from chat: `/voice out on|off` — a config-backed router command in the `/model` pattern. Contracts: `docs/contract/telegram.md` + the new key in `config_keys.rs`.
- Acceptance: e2e with a TTS fixture endpoint: `voice.out = true` → the settled answer arrives as a `sendVoice` carrying the fixture bytes and no duplicate text message; fixture down → today's text answer plus exactly one Notice; `/voice out off` restores text-only without a channel restart.
- Priority: P1 · effort M · risk 2

### 5 `/history summarize`
- Research basis: memory amnesia is the top churn driver (https://kilo.ai/openclaw/vs-hermes, https://medium.com/@sathishkraju/i-switched-from-openclaw-to-hermes-agent-heres-what-nobody-told-me-5f33a746b6ca), and Hermes' specific edge is "search-and-summarize" memory — FTS5 retrieval plus LLM summarization that recalls months of history without exceeding the model's context (https://getclaw.sh/blog/openclaw-vs-hermes-agent-feature-comparison-founders-2026). ka-agent has the FTS half (`/history` AND search); the summarize half is missing.
- Adaptation: `/history summarize <query>` and `/history summarize last <N>` as a branch of the history handler (`crates/ka-agent-channels/src/commands/memory.rs:142` `run_history`): run the existing FTS query (`runtime.history_search`, `memory.rs:179`) or take the last-N episodes, expand the matched turns as context reusing the `@turn:` expansion (`crates/ka-agent-channels/src/refs.rs:68,162`), and submit one turn on the background model (already accounted separately via `turns.model`) whose answer streams back normally with `source = "history"`; the reply opens with `summary of N turns:`. No new state; nothing enters stable memory outside the existing candidate→card flow.
- Acceptance: with seeded episodes, `/history summarize deploy` streams one background-model reply opening `summary of N turns:` that quotes content from the matched turns; zero matches → a `no history matches` reply and no turn is created.
- Priority: P1 · effort S · risk 1

### 6 Memory distillation preset
- Research basis: Hermes' "self-improving loop" — writing reusable knowledge from experience back into the agent — is its loudest differentiator (https://getclaw.sh/blog/openclaw-vs-hermes-agent-feature-comparison-founders-2026, corroborated at https://hundredtabs.com/blog/hermes-agent-vs-openclaw); R2 adds that users want memory that is curated and inspectable, not silent self-mutation (https://www.reddit.com/r/opencodeCLI/comments/1spztym/does_anyone_else_lose_trust_in_long_opencode).
- Adaptation: `ka-agent memory distill on <day> <HH:MM>` creates one weekly Agent cron whose curated prompt (the `briefing::PROMPT` pattern, special-cased by source in `scheduler.rs:346-353`) pulls the week's episodes through the item-5 summarize machinery, distills durable facts and preferences, and files each as a `memory_propose` candidate — operator-gated through the existing cards, so `docs/contract/memory.md`'s anti-pollution rule holds. `distill off|now` mirror `briefing` exactly (`apps/ka-agent/src/briefing_cli.rs:18-72`).
- Acceptance: `memory distill on sun 18:00` then `cron list` shows the weekly job; `memory distill now` produces ≥ 1 memory candidate card when the week has episodes and zero when it does not; no fact lands in stable memory without a card approval.
- Priority: P2 · effort M · risk 1

### 7 `/file <path>` — push a file to the phone
- Research basis: phone↔PC transfer is a recurring top affordance in the 87-use-cases thread — "Send a file via Telegram and copy it to my computer's desktop", "send a screenshot to my desktop from my mobile phone" (https://www.reddit.com/r/openclaw/comments/1sjr3ep/my_87_use_cases_for_openclaw_they_became_more/). ka-agent only pushes tool-*produced* files today (`AgentEvent::File` is emitted by the file tool).
- Adaptation: a router command `/file <path>` that resolves a workspace-relative (or granted) path, applies the same read rules as `@references` (protected files, grants — `crates/ka-agent-channels/src/refs.rs`), and emits `AgentEvent::File { path, caption }` — the event every surface already renders: Telegram photo/document choice (`telegram/surface.rs:594`), Matrix upload (`matrix/surface.rs:600`), TUI prints the absolute path (`apps/ka-agent/src/term/view.rs:466`). Refusal wording mirrors the `@ref` failure replies.
- Acceptance: from Telegram, `/file report.pdf` delivers the file with its path as caption; `/file <protected path>` returns the `@ref` refusal wording and emits no event; the TUI prints the resolved absolute path.
- Priority: P2 · effort S · risk 1

### 8 TUI diff pager on approval cards
- Research basis: OpenCode's TUI diff review is a praised differentiator (diff viewer + hunk navigation, v1.16.2 — https://www.morphllm.com/comparisons/opencode-vs-goose); first-hand, "Asked it to show a git diff on my phone. It kind of worked, but the UI was a mess" (https://www.reddit.com/r/openclaw/comments/1sjr3ep/my_87_use_cases_for_openclaw_they_became_more/). ka-agent's approval cards carry diffs, but the TUI shows only the card's truncated preview.
- Adaptation: in the TUI, when the newest pending approval card has a diff detail, `d` suspends raw mode through the existing `Suspended::begin()/end()` machinery (`apps/ka-agent/src/term/raw.rs:121-123`; the Ctrl-G pattern at `term/mod.rs:448-459`) and pipes the full diff to `$PAGER`; any key returns to the live area. The card footer gains `· d diff` when a diff exists (`apps/ka-agent/src/term/cards.rs`; card arming in `apps/ka-agent/src/chat.rs`).
- Acceptance: in a tmux TUI with a pending diff-bearing approval, `d` opens the pager showing the full diff; exiting the pager redraws the live area intact and the card remains answerable (`y`/`n`); a diff-less card shows no `d` hint and the key is ignored.
- Priority: P2 · effort S · risk 2

### 9 WhatsApp channel (wacli)
- Research basis: channel breadth is OpenClaw's headline edge — WhatsApp, Telegram, Discord, Slack, iMessage, Signal and browser chat through one assistant (https://getclaw.sh/blog/openclaw-vs-hermes-agent-feature-comparison-founders-2026) — and WhatsApp is the personal-messaging default (2B+ users) with a documented no-cost path: wacli speaks the WhatsApp-Web protocol, pairs by QR in under 15 minutes, needs no business verification, and is free per message (https://www.getopenclaw.ai/integrations/whatsapp). Discord/Slack are deliberately excluded (work-centric; Telegram + WhatsApp + Matrix cover the personal surfaces).
- Adaptation: new `crates/ka-agent-channels/src/whatsapp/` following the Matrix skeleton (channel + surface + client: `matrix/{channel,surface,client}.rs`); `ka-agent pair whatsapp` prints the QR code (the `pair.rs` deep-link precedent); identity gating identical to Telegram/Matrix — one bot, one operator per ADR-0001; text inbound becomes turns, voice notes go through the existing transcriber (`voice.rs`), documents through the attachment flow; `[whatsapp]` config section mirroring `[matrix]`.
- Acceptance: e2e against a wacli-protocol fake: QR pairing links the operator identity; an inbound text becomes a turn (👀 + reply); an inbound voice note becomes a transcribed turn; an unlinked sender receives nothing.
- Priority: P2 · effort L · risk 2

### 10 Skill usage stats + proposal nudge
- Research basis: Hermes' auto-generated skills — "when Hermes solves a complex problem, it can write a reusable skill document… over weeks this builds a custom skill library tuned to your actual workflows" — are its loudest differentiator (https://getclaw.sh/blog/openclaw-vs-hermes-agent-feature-comparison-founders-2026; https://hundredtabs.com/blog/hermes-agent-vs-openclaw names self-improvement Hermes' win). ka-agent already has the `skill_propose` card; missing are the model knowing *when* to propose and the operator seeing what is actually used.
- Adaptation: two small moves. (a) The bridge tool's description gains its trigger — propose after a ≥ 3-tool-call success that would recur (`kagent__skill_propose` registration `crates/ka-agent-channels/src/tools/mod.rs:164`, schema/handler `tools/assistant.rs:239-253`; the identity preamble already advertises the tool at `crates/ka-agent-runtime/src/runtime/identity.rs:15,84`). (b) `/skills` (`crates/ka-agent-channels/src/commands/skills.rs:5`) shows per-skill usage counts — a counter incremented at provisioning/enable time, stored beside the skills state.
- Acceptance: the tool description served over MCP contains the recurrence trigger; after two turns that enable a provisioned skill, `/skills` shows `used 2×` on its row and `0` on never-used rows.
- Priority: P2 · effort S · risk 1

### 11 Doctor zero-telemetry line
- Research basis: Hermes' zero-telemetry stance — "No data leaves your machine unless you explicitly configure an external service"; the "nothing phones home" guarantee "removes an entire category of compliance questions" — is a trust differentiator against OpenClaw's exposed-instance history (SecurityScorecard counted tens of thousands of publicly exposed instances) (https://getclaw.sh/blog/openclaw-vs-hermes-agent-feature-comparison-founders-2026, https://hundredtabs.com/blog/hermes-agent-vs-openclaw). ka-agent is already zero-telemetry; it just never says so.
- Adaptation: `doctor` (`apps/ka-agent/src/doctor.rs`) gains a line — `telemetry: none — outbound: your model provider (per turn), models.dev (registry, 24 h cache), github releases (weekly update check)` — enumerating exactly the three outbound destinations that exist in the code (provider calls at turn time; the models.dev fetch behind `models_live.rs`'s 24 h cache; the weekly release check at `apps/ka-agent/src/host.rs:251-274`), each with its nature and escape (local providers need no key; the registry serves from cache and `KA_AGENT_MODELS_REGISTRY` redirects it; the update check is the only GitHub call).
- Acceptance: `ka-agent doctor` prints the telemetry line naming all three destinations; `doctor --json` carries the same as a field; no other outbound destination exists in the tree (grep-verifiable at review).
- Priority: P2 · effort S · risk 1

## Rejected

- Skill marketplace / community skill installs — the category's demonstrated malware vector (341 malicious entries in one ClawHub audit); ka-agent's signed local plugins are the deliberate opposite.
- Group chats — ADR-0001 fixes one bot, one operator; group surfaces reopen pairing, audit, and abuse questions the design closed.
- Email connector — needs an OAuth/IMAP credential surface plus an abuse review (unsolicited outbound mail); revisit with a concrete operator ask.
- Discord/Slack channels — work-centric surfaces; Telegram + WhatsApp + Matrix cover the personal-assistant surfaces this product serves.
- Calendar write-back — a CalDAV write surface for little gain; the ICS digest + reminders already cover the observed ask.
- First-party image generation — MCP servers cover it today; no grant model exists for external paid media APIs.
- Heartbeat in the main conversation — OpenClaw users' most-documented pain; ka-agent's isolated crons are precisely the fix they ask for, and item 1 completes it with zero-token checks.
