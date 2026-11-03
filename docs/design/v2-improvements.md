# v2 improvement audit — 2026-11-03

Baseline: v0.12.3, `main` @ `2aa4c7c`, `cargo build -p ka-agent` + `cargo test
--workspace` green (497 passed, 0 failed, 39 suites) before any audit work.

## Method

Four waves. Wave 0 built the binary, recorded the green baseline, read the four
behavior-defining document sets (`docs/design/v1-redesign.md`, `README.md`, the
15 `docs/contract/*.md`, `docs/ops/dogfooding.md`) and enumerated the live
surface inventory (CLI commands in `apps/ka-agent/src/cli.rs`, chat commands in
`crates/ka-agent-channels/src/commands/mod.rs`, 44 `KeySpec` rows in
`crates/ka-agent-core/src/config_keys.rs`, web routes in
`crates/ka-agent-web/src/lib.rs:475-541`, term keys in
`apps/ka-agent/src/term/keys.rs` + `docs/contract/terminal.md`). Wave 1 ran six
parallel area audits (A terminal, B Telegram, C onboarding, D configurability,
E web panel, F cross-cutting), each driving the real binary
(`./target/debug/ka-agent`) under throwaway `KA_AGENT_DATA_DIR=$(mktemp -d)`
roots with the scripted-speaker `--scenario` engine (no network, no keys),
tmux/pty captures for the TUI, and curl for the web panel; the real instance
was never touched. Evidence rule: every finding below is either reproducible
with the recorded command or grounded at an exact `path:line` (verified by
reading); 17 of the 59 findings — including 5 picked uniformly at random
(`shuf`: C6, D3, A8, E10, A5) — were re-verified first-hand by the lead after
the scouts reported. Scoring: priority = impact (1-3, 3 = blocks or delights
daily) × frequency (1-3, 3 = every session); bands P0 ≥ 6, P1 3-5, P2 ≤ 2;
tie-break lower effort (S < 1 day, M = days, L ≥ week), then lower risk (2 =
touches approvals/security/engine pin). Verified non-findings are recorded in
Rejected so seeds are never re-audited. This document is a catalog only —
nothing here is implemented.

## Top 10

| id | title | priority | effort | impact |
|---|---|---|---|---|
| A1 | TUI drops every command reply and operational notice | P0 | M | `/help`, `/status`, `/remind` print nothing; pause/budget/key remedies invisible on the flagship surface |
| A2 | Ctrl-R panics and kills the session | P0 | S | one press of a documented key exits 101, losing the session |
| A4 | Approval cards render inside the live area and get overwritten | P0 | S | every ask-mode card (the security moment) renders corrupted |
| B1 | Inbound photo/document caption is silently dropped | P0 | S | a screenshot plus "what's wrong here?" loses the question |
| F2 | No Retry affordance after rate_limit/network; interrupted turns never re-run | P0 | M | the operator retypes the whole prompt from memory |
| A6 | Tool output invisible; Ctrl-O detail toggle is dead code | P0 | M | `✓ bash 19ms` is all the operator ever sees of what ran |
| A5 | Multi-candidate Tab completion never inserts; announcements erased | P1 | S | completion beyond one match is unusable and corrupts layout |
| E4 | History search is phrase-only — multi-word queries return nothing | P1 | S | `?q=deploy+cache` → 0 hits; search feels broken |
| F5 | Piped `chat` lingers 120 s after stdin EOF | P1 | S | every scripted/CI chat pays a silent 2-minute tail |
| C1 | Help creep: 20 visible commands vs the ≤ 16 promise | P1 | S | the first screen a new operator reads is again a wall |

## Findings — by area (A–F)

### A1 The inline TUI drops every command reply and operational notice
- Evidence: tmux TUI session (`tmux new-session -d -s t "env KA_AGENT_DATA_DIR=$T/data ./target/debug/ka-agent chat --config $T/c.toml --scenario $T/s.json"`), `tmux send-keys -t t '/help' Enter` → capture shows only the echoed `» /help`, no output; same for `/status`, `/model`, `/stop` (piped path prints `· nothing to stop` for the latter). A scripted auth failure shows only `! [auth] bad key` while piped prints the remedy `· auth failure — paused; fix the provider key and /resume`. Root cause: `apps/ka-agent/src/term/view.rs:267-377` `absorb()` has no arms for `Reply`, `Note`, `Notice`, `Session`, `Inbound`, `ApprovalSettled`, `CronFinished` — they hit `_ => {}` at view.rs:374 (`grep -n "Notice" apps/ka-agent/src/term/*.rs` → 0). Re-verified first-hand by the lead (tmux capture on 2026-11-03). Cross-ref F2's notice list.
- Impact: as an operator in the TUI, every command except free chat looks broken, and budget-pause/degraded/cron-failure/key-remedy notices never appear.
- Proposal: in `apps/ka-agent/src/term/view.rs` `absorb()` add arms mirroring `chat.rs::render()` for `Reply` (through the shared Markdown take), `Note`, `Notice`, `Inbound` (non-console sources), `Session` (Paused/Degraded → status badge in `view.rs:68-103`), `ApprovalSettled`, `CronFinished`.
- Acceptance: in a tmux TUI session `/help` prints the command list, `/stop` (idle) prints `· nothing to stop`, and a scripted auth failure prints the `/resume` remedy line; `grep -c 'Notice' apps/ka-agent/src/term/view.rs` ≥ 1.
- Priority: P0 · effort M · risk 1

### A2 Ctrl-R panics and kills the whole session
- Evidence: tmux TUI session with history, `tmux send-keys -t t C-r` → pane dies; stderr: `thread 'main' panicked at apps/ka-agent/src/term/mod.rs:297:41: Cannot block the current thread from within a runtime`, exit 101. `mod.rs:297` is `self.key_rx.blocking_recv()` inside `reverse_search()`, called from async `next_action()`. `docs/contract/terminal.md:48` documents Ctrl-R. Re-verified first-hand by the lead (full panic captured).
- Impact: one press of a documented key instantly crashes the client, losing the session and any turn in flight.
- Proposal: in `apps/ka-agent/src/term/mod.rs` `reverse_search()` replace the blocking loop with an async state machine serviced by the existing `key_rx.recv().await` (a `searching: Option<…>` field on `Terminal`), or `tokio::task::block_in_place`.
- Acceptance: in a tmux TUI session with prior history, `C-r` then `hel` + Enter leaves the process alive (`tmux list-sessions` shows it) and the draft holds the matched line; existing decoder tests still green.
- Priority: P0 · effort S · risk 1

### A3 Ctrl-G external editor races the client for stdin — effectively unusable
- Evidence: live TUI, draft text then `C-g` → vi opens; 1-2 Escapes (and Ctrl-C) never leave INSERT mode (`capture-pane` shows `-- INSERT --`); a burst of 6 Escapes lets one through; `:wq` returns with an empty draft. The stdin reader thread (`apps/ka-agent/src/term/keys.rs:160-217`) keeps reading while the editor runs; `Suspended::begin()` (`apps/ka-agent/src/term/raw.rs:124-127`) only restores termios. `docs/contract/terminal.md:55` documents Ctrl-G.
- Impact: the only structure-preserving multi-line input path steals the operator's keystrokes and starves $EDITOR.
- Proposal: in `apps/ka-agent/src/term/keys.rs` add a cooperative pause (an `Arc<AtomicBool>` or `tokio::sync::Notify`) the reader checks before `read()`; `edit_in_editor` (`mod.rs:321-342`) sets it around `Command::status()` and drains `key_rx` afterwards.
- Acceptance: with `EDITOR=vi`, `C-g`, type `A appended`, one `Escape`, `:wq` → back in the TUI with the draft containing `appended`.
- Priority: P2 · effort M · risk 1

### A4 Approval cards render inside the live area and get overwritten
- Evidence: mode=ask + scripted `bash rm -rf …` call → capture shows the card's first line glued to the prompt row (`» ┌ approval d3aa7736 …`) and the `└ y allow · n deny · s …` footer erased by the next redraw. `apps/ka-agent/src/chat.rs:662-664` calls `show_event` (ends with `draw()`) then `cards::print_card` which `println!`s at the cursor; the next redraw's row count was measured before the card existed. `docs/contract/terminal.md:69-72` says cards print as permanent blocks above the live area. Code order re-verified first-hand by the lead.
- Impact: in the default ask mode every approval card renders corrupted exactly at the security-critical moment, hiding the y/n/s answer key.
- Proposal: render cards inside the event flush: move the `cards::print_card(&env)` call from `chat.rs:664` into `show_event` (`apps/ka-agent/src/term/mod.rs:352-374`) between `begin_permanent()` and `draw()`.
- Acceptance: same tmux approval run: `┌ approval …` starts at column 0 on its own line, the `└ y allow …` footer is intact after answering `y`, and no card line carries a `»` prefix.
- Priority: P0 · effort S · risk 1

### A5 Multi-candidate Tab completion never inserts; toggle announcements and candidate lists are erased
- Evidence: `/m` + Tab ×3 → editor still reads `/m`; the candidate list is printed with a transient `print!` (`apps/ka-agent/src/term/mod.rs:273-276`) then erased by `self.draw()` at mod.rs:243, and each wrap leaves a stale status row (capture showed four stacked `☥ Ka · …` lines after three Tabs). `replace_word` runs only in the single-match branch (mod.rs:254-255); `completion_idx` cycles (mod.rs:259) but the cycled candidate is never inserted. Ctrl-O/Ctrl-T announcements print the same way (mod.rs:190-203) and are equally invisible. Code re-verified first-hand by the lead.
- Impact: completion beyond a single match is unusable, toggle state changes give no feedback, and the layout corrupts.
- Proposal: in `apps/ka-agent/src/term/mod.rs` route transient feedback through the permanent path (`begin_permanent()` + `println!` before `draw()`), and make repeated Tab insert the cycled candidate via `replace_word(&self.completions[self.completion_idx].insert)`.
- Acceptance: `/m` + Tab shows `/mode … /memory … /model …` as a line that survives the next keystroke; each further Tab replaces the word with the next candidate; after 3 Tabs `capture-pane` shows exactly one status line.
- Priority: P1 · effort S · risk 1

### A6 Tool output is invisible; the Ctrl-O "tool details" toggle is dead code
- Evidence: scripted `bash echo tool-output-line-1; echo tool-output-line-2` → capture shows only `  ✓ bash 19ms`; output appears nowhere. `apps/ka-agent/src/term/view.rs:223-231` renders name+ms only; failures keep the error's first line truncated to 60 columns; no expand key exists in `term/`. `view.verbose` set at `mod.rs:191` is never read (absorb gets `term.verbosity`, the Ctrl-T flag — `mod.rs:354`), so `docs/contract/terminal.md:53`'s "Ctrl-O | toggle tool detail rows" does nothing.
- Impact: the operator cannot verify what a command did without re-running it — trust and debuggability suffer on every tool turn.
- Proposal: in `apps/ka-agent/src/term/view.rs` emit the tool detail line for `ToolStarted` and the stored output rows for `ToolFinished` when details are on (wire `view.verbose` into `absorb`), plus an expand action in `mod.rs` `next_action` that re-prints the last tool's full output as a permanent block.
- Acceptance: with Ctrl-O on, the scripted echo turn's transcript contains `tool-output-line-1`; the expand key on the settled row prints both lines.
- Priority: P0 · effort M · risk 1

### A7 Background stderr corrupts the TUI; plain `chat` tries to serve the web panel by default
- Evidence: first run without `[web]` in config, pane shows `» ka-agent: web UI disabled: bind: bind 127.0.0.1:8480: Address already in use (os error 98)` wrapping mid-live-area — the `eprintln!` at `apps/ka-agent/src/host.rs:350` fires from a spawned task after the TUI started. `Web::default()` is `enabled: true` (`crates/ka-agent-core/src/config.rs:181-190`) and `Backend::local` passes `web_port: None` (`chat.rs:66`), while `cli.rs:126` documents `--web-port` "(default: disabled)". Observed first-hand by the lead during the A2 repro (same pane).
- Impact: on any machine where 8480 is taken (second profile, another service) the chat's first screen is garbled with terminal-control-incompatible text.
- Proposal: route background-task failures through `AgentEvent::Notice` in `apps/ka-agent/src/host.rs` instead of `eprintln!`; reconcile the web default between `config.rs:181-190`, `cli.rs:126` and `chat.rs:66` (a client `chat` should not bind the panel port).
- Acceptance: with the web port occupied, a tmux chat pane contains no "web UI disabled" text below the banner; the failure arrives as a `· ` notice line.
- Priority: P2 · effort S · risk 1

### A8 Status line shows `plan 0/0` when no plan exists
- Evidence: every capture of every scripted turn (no todos) shows `☥ Ka · ctx 0% · $0.00 · plan 0/0`. `view.rs:339-342` stores `Some((done, items.len()))` for any `Plan` event including the engine's empty initial list; `view.rs:92-94` prints whenever `plan.is_some()`. `docs/contract/terminal.md:28-30`: "`plan done/total` **when the model published one**". Code re-verified first-hand by the lead.
- Impact: permanent zero-information noise teaches the operator to ignore the plan meter.
- Proposal: in `apps/ka-agent/src/term/view.rs` ignore empty plans at the `Plan` arm (store `None` when `items.is_empty()`), or guard the status push with `total > 0`.
- Acceptance: a scripted turn with no todos shows a status line without `plan`; a turn publishing 2 todos shows `plan 0/2` → `plan 2/2`.
- Priority: P1 · effort S · risk 1

### A9 Key bindings are undiscoverable in-app; the Esc binding is undocumented
- Evidence: the banner (`apps/ka-agent/src/chat.rs:406-414`) names only `/help`, Ctrl-C, Ctrl-D; `/help` (`help_text`, `crates/ka-agent-channels/src/commands/mod.rs:340-356`) lists only slash commands. No in-app list of Ctrl-O/T/G/R/L, Tab, Shift-Enter; Esc→clear-completions (`mod.rs:173`) is absent from `docs/contract/terminal.md`'s key table too.
- Impact: a new operator never learns the toggles, completion, or editor keys without reading the docs.
- Proposal: add a `/keys` command to `COMMANDS` (`crates/ka-agent-channels/src/commands/mod.rs`) printing the terminal key table, append `· /keys` to the banner, and document Esc in `docs/contract/terminal.md`.
- Acceptance: piped `/keys` prints lines containing `Ctrl-O` and `Tab`; the TUI banner contains `/keys`.
- Priority: P1 · effort S · risk 1

### A10 Pasted newlines silently become spaces
- Evidence: `tmux set-buffer -t t $'line one\nline two\nline three'; tmux paste-buffer -p -t t` → draft shows one collapsed line. `apps/ka-agent/src/term/editor.rs:47-54` maps `\n|\r` → `' '` on every insert; `Key::Newline` inserts a space (`mod.rs:149-152`); the only structure-preserving path (Ctrl-G) is broken (A3). `docs/contract/terminal.md:41` mandates the fold for typed newlines.
- Impact: pasting logs/code/YAML destroys the line structure the model may need, with no warning.
- Proposal: keep pastes verbatim in `editor.rs` (bracketed-paste inserts real newlines; the multi-line draft already wraps), fold only typed Shift-Enter; update `terminal.md:41`.
- Acceptance: pasting the 3-line buffer yields a draft containing `\n` when submitted (scripted answer echoes it).
- Priority: P1 · effort M · risk 1

### A11 Kills are unrecoverable: no yank, Ctrl-Y unbound
- Evidence: `apps/ka-agent/src/term/mod.rs:177-185` discard the strings returned by `kill_to_end/kill_to_start/kill_word` (`editor.rs:110-128`); `Key::Ctrl('y')` falls into the no-op `Key::Ctrl(_) => {}` at mod.rs:240.
- Impact: a fat-fingered Ctrl-U destroys a long draft with no undo.
- Proposal: one-slot kill ring in `Editor` (`last_kill: String`), bind Ctrl-Y in `mod.rs`, add a row to `terminal.md`'s key table.
- Acceptance: type `abc def`, Ctrl-U, Ctrl-Y → draft is `abc def` again.
- Priority: P2 · effort S · risk 1

### A12 NO_COLOR ignored in the TUI; no theme/mouse/key-remap surface
- Evidence: `NO_COLOR=1` TUI run still emits SGR (`tmux capture-pane -e` shows `^[[1m»…`, `^[[2m☥ Ka…`); the check exists only on the piped path (`chat.rs:207`). No `theme|color|mouse|keymap` hits in `crates/ka-agent-core/src/config_keys.rs` (44 rows) or `config.rs`; raw mode enables only bracketed paste + focus (`raw.rs:30-50`).
- Impact: operators who need low-chrome output (NO_COLOR convention, accessibility) or different keys have no recourse.
- Proposal: honor `NO_COLOR` in `apps/ka-agent/src/term/paint.rs` and `view.rs`'s inline SGR; defer themes/remaps/mouse unless asked.
- Acceptance: `NO_COLOR=1` TUI session captured with `capture-pane -e` contains no `^[[2m`/`^[[1m` sequences.
- Priority: P2 · effort S · risk 1

### B1 Inbound photo/document caption is silently dropped
- Evidence: `crates/ka-agent-channels/src/telegram/channel.rs:396-411` — photo/document is saved and answered `saved {rel} — reference it with @{rel} …`, then `return`s at :410; text extraction at channel.rs:413 reads only `message["text"]`, never `message["caption"]`. `attachments.rs:17-48` never reads the caption; no test covers a captioned photo. Re-verified first-hand by the lead.
- Impact: I send a screenshot with the caption "what's wrong here?" and the question silently vanishes — only the "saved" ack arrives.
- Proposal: in `dispatch_message` (channel.rs:396-411) read `message["caption"].as_str()` after a successful save; when non-empty submit the turn with `{caption}\n(@{rel})` via `runtime.handle_message` (👀 + normal path); keep the plain ack for caption-less media.
- Acceptance: e2e (pattern of `voice_note_becomes_a_turn_with_its_transcript`, `apps/ka-agent/tests/telegram_e2e.rs:328`): push a photo with `caption:"what is this?"` → `turns.inbound_text` contains both the caption and `@attachments/`.
- Priority: P0 · effort S · risk 1

### B2 Unsupported inbound media is dropped with zero feedback
- Evidence: `channel.rs:387-415` — after the `voice|audio` arm (:387) and `photo|document` arm (:396), everything else falls to `let Some(text) = … message["text"].as_str() else { return; }` (:413-415). A video note / video / animation / sticker / location / contact message has no `text` → silent return: no reply, no 👀 (fires only in `NotACommand`, :431), no stderr, no audit. `grep -n 'video_note|sticker|"video"|animation|location' crates/ka-agent-channels/src` → no handler. Re-verified first-hand by the lead.
- Impact: I send a video note (a primary Telegram input) and the assistant ignores it — the chat looks broken with no clue why.
- Proposal: in `dispatch_message`, before the `text` extraction, catch messages carrying `video|video_note|animation|sticker|location|contact` with no usable text and reply `! [media] {kind} isn't supported yet` (same shape as the attachment/voice error replies).
- Acceptance: e2e: push a `video_note` update → a captured `sendMessage` contains `video note`; no `turns` row is created.
- Priority: P1 · effort S · risk 1

### B3 Deep-link pairing: `t.me/<bot>?start=<code>` would make pairing one tap
- Evidence: `apps/ka-agent/src/pair.rs:60,75` prints "Send this to your bot: /pair {code}"; inbound, `/start <payload>`'s payload is ignored (`channel.rs:336-355` unlinked → hourly `START_REPLY`; linked → `/help` alias). Bot API deep-linking allows payloads ≤ 64 chars `[A-Za-z0-9_-]` (core.telegram.org/bots/features#deep-linking); the `XXXX-XXXX` code fits (`pairing.rs:19-21`) and `redeem` already accepts dash-optional codes (`pairing.rs:110-173`).
- Impact: on a fresh install I must hand-copy a code into the chat instead of tapping one link the CLI prints.
- Proposal: (1) `pair.rs` and `setup/write.rs:263` also print `https://t.me/{bot}?start={code}`; (2) in `unlinked_private` (`channel.rs:317`) route `/start <payload>` through `pairing::redeem` — identical semantics (10-min TTL, 5 attempts, constant-time compare), no new authority, ADR-0001 untouched.
- Acceptance: channels e2e: `/start XXXX-XXXX` from an unlinked uid with a live code → identity linked + welcome sent; wrong payloads burn attempts exactly like `/pair`.
- Priority: P2 · effort S · risk 2

### B4 `sendChatAction` is hardcoded to `typing` — no upload actions when files go out
- Evidence: `crates/ka-agent-channels/src/telegram/client.rs:226-233` always sends `{"action":"typing"}`; the only caller is the 4 s TurnStarted loop (`surface.rs:408-413`); `grep -n 'upload_' crates/ka-agent-channels/src` → none, although Bot API has `upload_photo`/`upload_document` for exactly this.
- Impact: when the assistant sends a 40 MB file I see "typing…" then minutes of nothing instead of an upload indicator.
- Proposal: parameterize `TelegramClient::send_chat_action`; in the `AgentEvent::File` arm (`surface.rs:586-603`) fire one `upload_photo`/`upload_document` (same split as `photo_mime`) before `deliver_file`.
- Acceptance: e2e: a File event for a `.png` → a captured `sendChatAction` with `"action":"upload_photo"` precedes `sendPhoto`.
- Priority: P2 · effort S · risk 1

### B5 Bot profile never set: empty "What can this bot do?" and chat-list blurb
- Evidence: the channel registers only `setMyCommands` (`channel.rs:165-167`, `client.rs:235-255`); `grep -n 'setMyDescription|setMyShortDescription|setMyName|setChatMenuButton|language_code' crates/ka-agent-channels/src` → no matches. Bot API `setMyDescription`/`setMyShortDescription` (≤512/≤120 chars) fill the empty-chat screen and the chat list.
- Impact: before my first message the bot looks unfinished — empty description, silent chat-list preview.
- Proposal: beside the `setMyCommands` call in `run` (`channel.rs:165`), best-effort `setMyDescription`/`setMyShortDescription` with a one-line summary (same "a refusal never disables the channel" policy).
- Acceptance: e2e: after channel start the fake captures `setMyDescription` and `setMyShortDescription` with non-empty `description`.
- Priority: P2 · effort S · risk 1

### B6 Outbound video/GIF goes as a document (no inline playback)
- Evidence: `surface.rs:697-728` (`deliver_file` + `photo_mime`): only `sendPhoto` (jpg/jpeg/png/webp ≤10 MB) vs `sendDocument` — mp4/webm/gif take the document path. Bot API `sendVideo`/`sendAnimation` (50 MB) play inline.
- Impact: when the agent produces a screen recording or GIF I must download it instead of watching it in the chat.
- Proposal: extend the `deliver_file` match: `.mp4|.m4v|.webm|.mkv` ≤ 50 MB → `sendVideo`, `.gif` → `sendAnimation`; keep the document fallback on any refusal.
- Acceptance: a workspace `.mp4` File event → captured method `sendVideo` with the file in part `video`.
- Priority: P2 · effort S · risk 1

### B7 Media groups (albums): one ack per photo, no grouping
- Evidence: `attachments.rs:17-48` keys off `photo`/`document` only; `grep -n 'media_group' crates/ka-agent-channels/src` → no matches. Telegram delivers an N-photo album as N messages sharing `media_group_id`, each getting its own `saved photo-xxxx.jpg — …` reply (channel.rs:396-411).
- Impact: I send a 6-photo album and get 6 separate acks plus 6 refs to paste one by one.
- Proposal: group by `media_group_id` (a short debounce map in `TelegramChannel`): save all variants, reply once listing every `@attachments/…` ref; singles keep today's behavior.
- Acceptance: e2e: 3 updates sharing one `media_group_id` → exactly 1 ack `sendMessage` naming 3 refs, 3 files on disk.
- Priority: P2 · effort M · risk 1

### C1 Help creep: 20 visible commands vs the ≤ 16 promise
- Evidence: `./target/debug/ka-agent --help | sed -n '/^Commands:/,/^Options:/p' | grep -cE '^  [a-z]+'` → 20, plus a 17-command footer (`apps/ka-agent/src/cli.rs:654-657`). `docs/design/v1-redesign.md:174-176` acceptance: "`ka-agent --help` lists ≤ 16 commands". Re-verified first-hand by the lead (count 20).
- Impact: the everyday set v1 B1 curated has eroded (notify, hooks, persona, usage, memory, mcp surfaced) — the first screen a new operator reads is again a wall.
- Proposal: re-tuck the least-daily commands into the `ADVANCED_HELP` footer in `apps/ka-agent/src/cli.rs` (parsing untouched), restoring ≤ 16 visible rows.
- Acceptance: the count command prints ≤ 16 and every demoted command still parses (`ka-agent notify --help` exits 0).
- Priority: P1 · effort S · risk 1

### C2 Fresh-root `status` points at `service install` before `setup` exists
- Evidence: `T=$(mktemp -d); KA_AGENT_DATA_DIR=$T ./target/debug/ka-agent status` → `Ka Agent is not running — start it with \`ka-agent service install\` (background) or \`ka-agent run\`` while `chat` on the same root correctly says "isn't set up yet — run `ka-agent setup`"; exit codes also split (no-arg 2 vs chat/status 1). Re-verified first-hand by the lead.
- Impact: a new user who runs `status` first (one of the four commands README advertises) is told to install a service for an instance with no model or key.
- Proposal: in the status command path, run the same config-existence guard `chat` uses before the not-running message; align the exit code.
- Acceptance: on an empty `KA_AGENT_DATA_DIR`, `ka-agent status` prints the setup hint, mentions no `service install`, and exits 1.
- Priority: P2 · effort S · risk 1

### C3 `import` is undiscoverable: absent from `--help`, the footer, and setup
- Evidence: `cli.rs:655-656` footer lists 16 advanced commands — no `import`; the wizard never offers it (`setup/mod.rs:189-209`); documented only at `README.md:237`. It works: `HOME=$T/home ka-agent import agent hermes --dry-run` → `memory: 1 fact(s) … / skills: 1 (demo) / dry run — nothing was written`.
- Impact: a user migrating from hermes/openclaw/claude-code never learns the one command that carries their memory and skills over.
- Proposal: add `import` to `ADVANCED_HELP` (cli.rs:654); after a fresh write in `setup/write.rs::post_write`, print one hint line when a known source (`~/.hermes`, `~/.claude`, …) is detected.
- Acceptance: `ka-agent --help` contains `import`; a fresh wizard run on a HOME containing `.hermes/` ends with a line mentioning `ka-agent import`.
- Priority: P2 · effort S · risk 1

### C4 `dashboard` misdiagnoses a web bind failure as "the web UI is off"
- Evidence: with 127.0.0.1:8480 held by another process, the daemon logs `web UI disabled: bind: … Address already in use` but `ka-agent dashboard --no-open` prints `the web UI is off — ka-agent config set web.enabled true` — a no-op (web defaults true, `config.rs:175-181`). `apps/ka-agent/src/dashboard.rs:25-28` treats any missing `web.url` meta as "off".
- Impact: when the port is taken (second profile, anything on 8480) the operator is told to enable an already-enabled flag, changes nothing, and has no path to the real cause.
- Proposal: in `dashboard.rs` distinguish enabled-but-unbound: "the web UI failed to start (port busy?) — try `ka-agent config set web.port <n>`, then `ka-agent restart`".
- Acceptance: with the port held and `web.enabled=true`, `ka-agent dashboard --no-open` mentions the port, not "web UI is off".
- Priority: P2 · effort S · risk 1

### C5 doctor's service/backup/logs lines describe the default-profile instance, not the data dir in scope
- Evidence: on a fresh `KA_AGENT_DATA_DIR=$(mktemp -d)` root while the machine's real default service runs, `doctor` prints both `✓ background service installed` and `! service is active but the daemon does not answer` (and `! no backup yet — … install the service`). `doctor.rs:333` checks the temp root's admin socket while `:341-352` checks `service::is_installed()/is_active()` for the profile's systemd unit (`service.rs:877-879`); `ka-agent logs` tails the default unit regardless of data dir (`service.rs:490-507,996-999`). Merged with F7 (same root cause). Cross-ref D7.
- Impact: anyone doctoring a second profile/container/test root is told a healthy service is broken and sent chasing another instance's logs.
- Proposal: when `KA_AGENT_DATA_DIR` (or a non-default profile) is in scope, qualify or drop the service/backup lines in `doctor.rs`, and make `logs` name the unit it reads.
- Acceptance: `KA_AGENT_DATA_DIR=$(mktemp -d) ka-agent doctor` prints no `service is active but the daemon does not answer` line while another root's service runs.
- Priority: P2 · effort S · risk 1

### C6 `update` HTTP errors surface as serde parse errors
- Evidence: fixture returning GitHub's rate-limit body with status 403 (`KA_AGENT_UPDATE_API=http://127.0.0.1:18999`): `ka-agent update check` → `release metadata: missing field \`tag_name\` at line 1 column 139`. `http_get` (`apps/ka-agent/src/update_cli.rs:52-62`) never checks `resp.status()`. Code re-verified first-hand by the lead.
- Impact: an operator hitting GitHub's 60 req/h unauthenticated limit (or a bad `--version`) sees a parser error naming neither URL, status, nor rate limiting.
- Proposal: in `http_get`, on non-2xx read the body's `message` and fail with `"{status} {url}: {message}"`; keep the (good) checksum wording.
- Acceptance: with the 403 fixture, `update check` stderr contains `403` and the body's message, and no `tag_name`.
- Priority: P2 · effort S · risk 1

### D1 `[[grants.path]]` is only hand-editable; `config unset grants.path` wipes the array silently
- Evidence: `ka-agent config set grants.path '[…]'` → `only flat arrays can be set …; arrays of tables are edited with ka-agent config edit` (rc=1; `config_file.rs:67`); `config unset grants.path` → `removed grants.path` (rc=0, no confirmation) and every entry is gone (both reproduced by the lead). `ka-agent grant add|rm|list` manages only state.db scopes (`grant.rs:19-54`); `grants.path` is `web: false` (`config_keys.rs:445-452`).
- Impact: an operator granting write access to a project dir (deny-by-default security semantics) must hand-write TOML, and one careless `config unset` deletes all path grants with no prompt.
- Proposal: add `ka-agent grant path add <dir> [--access allow|ask|deny]` / `grant path rm` / `grant path list` in `apps/ka-agent/src/grant.rs` writing `[[grants.path]]` via `config_file::read_doc/write_doc` (the `mcp add` pattern, `mcp_cli.rs:83-134`); make `unset` on a table list what it removes.
- Acceptance: `grant path add /tmp/proj` then `config get grants.path` shows the entry; `grant path rm /tmp/proj` removes only that row.
- Priority: P1 · effort S · risk 2

### D2 Registry bounds contradict `validate()`; the web form enforces the wrong ones
- Evidence: `config schema` prints `approval_ttl_secs · int 30..=86400` (`config_keys.rs:142-152`) but `config set approval_ttl_secs 10` succeeds — `validate()` allows `1..=86400` (`config.rs:635-648`); the web form enforces the registry bound (`crates/ka-agent-web/src/pages.rs:1012-1015`), so the web refuses values the CLI and file accept. `examples/config/ka-agent.toml:30` documents `1..=86400`, agreeing with the code against the registry. Both sides reproduced by the lead.
- Impact: the schema stops being the single source of truth; a value that works in the file is refused in the panel.
- Proposal: pick one authority per key — validate against `KeySpec` bounds in `DaemonConfig::validate` or widen the registry rows (`config_keys.rs:142-171`, `config.rs:635-648`).
- Acceptance: `config schema` ranges equal the edge values `config set` accepts for every int key.
- Priority: P2 · effort S · risk 1

### D3 `calendar.digest_time` claims "applies now" but an existing digest keeps the old time
- Evidence: registered `Apply::Live` (`config_keys.rs:339-346`); the time is baked into the cron only at `/calendar add` (`crates/ka-agent-channels/src/commands/schedule.rs:303-314`, `at_local: Some(digest_time)`); `reload_config_locked` (`crates/ka-agent-runtime/src/runtime/config.rs:47-86`) swaps budgets/timezone/cfg and stops idle workers but touches no cron rows. Reload path re-verified first-hand by the lead.
- Impact: the operator moves the digest 08:00 → 07:30, is told "applied live", and the digest silently keeps firing at 08:00.
- Proposal: in `reload_config_locked`, when `calendar.digest_time` changed and a `CronMode::NoAgent("calendar")` cron exists, rewrite its `at_local`/next fire; or demote the key's apply label with a "re-run /calendar add" note.
- Acceptance: with a digest cron at 08:00, `config set calendar.digest_time 07:30` then `ka-agent cron list` shows 07:30.
- Priority: P2 · effort S · risk 1

### D4 No `mcp test` — MCP servers are never probed for reachability
- Evidence: `ka-agent mcp --help` lists only `list|add|rm`; `mcp list` prints name + command/URL + secrets with no connection attempt (`mcp_cli.rs:42-69`). Hooks have a verification command (`ka-agent hooks test`).
- Impact: a typo'd URL or missing `npx` is discovered mid-conversation as missing tools instead of at setup time.
- Proposal: `ka-agent mcp test [name]` in `apps/ka-agent/src/mcp_cli.rs`: spawn/open the server, initialize, list tools, print the count (reusing the worker's MCP init path in `crates/ka-agent-port`).
- Acceptance: `mcp test` against a dead URL exits 1 naming the server and error; against a live one prints the tool count.
- Priority: P2 · effort M · risk 1

### D5 `config set ka.engine_layer` accepts a nonexistent file; the failure waits for the next turn
- Evidence: `config set ka.engine_layer /nonexistent/ka.toml` → `saved … / applied live` (rc=0). Set-time checks cover only model/fallback (`config_cli.rs:95-115`); `validate()` never inspects the path (`config.rs:577-797`); `parse_layer` runs at worker spawn (`crates/ka-agent-port/src/worker.rs:328`).
- Impact: the operator is told the layer applied, then the next message dies with an engine error.
- Proposal: extend set-time checks (`apps/ka-agent/src/config_cli.rs`): read the file and run the engine layer parse before anything is written; refuse on absence/parse error.
- Acceptance: `config set ka.engine_layer /nonexistent` exits 1 with the read error and leaves the config unchanged.
- Priority: P2 · effort S · risk 1

### D6 `config set` prints "applied live" for Worker keys while the schema says "next turn"
- Evidence: reproduced mid-turn on a scripted daemon: `config set mode readonly` → `applied live: mode`; the schema row says "applies next turn" (`config_keys.rs:134-141`); `reload_config_locked` only stops an *idle* worker (`runtime/config.rs:66-84`), so the running turn finished in `ask`.
- Impact: the operator believes mode/model switched under the running turn; the two config surfaces disagree on vocabulary.
- Proposal: in `apply_live` (`config_cli.rs:50-93`) label Worker-class keys with their registry wording (`applied (next turn): mode`), reserving "applied live" for `Apply::Live`.
- Acceptance: during a running turn, `config set mode readonly` prints `applied (next turn): mode`.
- Priority: P2 · effort S · risk 1

### D7 `-c/--config` edits a file outside the ambient profile, but reload/restart advice follows the ambient profile
- Evidence: daemon on a temp root, `config set web.enabled true` → `restart needed — run ka-agent restart`, resolving the *real* default-profile unit (`restart_hint` `config_cli.rs:41-48` → `service::is_installed()` → `profile_name()`, `service.rs:877-879`); `apply_live`'s admin check uses `Paths::default_root()` (`config_cli.rs:118`), following `KA_AGENT_DATA_DIR`, not the `-c` file. Cross-ref C5.
- Impact: following the advice restarts the wrong (production) instance, or "applied live" reloads a daemon that never reads the edited file.
- Proposal: thread the effective profile/config location through `config`/`mcp` actions so `restart_hint` names the exact unit (`ka-agent -p <profile> restart`) and `apply_live` resolves the admin socket from the same root as the config file.
- Acceptance: with `KA_AGENT_DATA_DIR` pinned to an empty root, `config set web.enabled true` prints no `ka-agent restart` advice naming an installed unit of another root.
- Priority: P2 · effort M · risk 1

### D8 No operator-language setting — every operator-facing string is English
- Evidence: no `language|locale` key in `config.rs`/`config_keys.rs` (all 44 rows listed); channel chrome is hardcoded English (`mod.rs:33` `START_REPLY`, `pairing.rs:26-27`, "saved …", voice hints, every command reply); `grep -n 'language_code|i18n|locale' crates/ka-agent-channels/src` → none. Merged with B10 (same gap from the Telegram side).
- Impact: a Spanish-first operator gets English UI chrome on every interaction and must resort to persona surgery to change the model's language.
- Proposal: register `language` (`Kind::Enum(["en","es"])`, default `en`, `Apply::Live`) in `config_keys.rs`, feed it into the per-turn preamble so replies follow it, and add a minimal en/es string table for channel-authored labels; confirm demand for the string table before building it.
- Acceptance: `config set language es` then a scripted turn asserts the Spanish preamble in a unit test; a pushed `/start` under `locale = "es"` answers the Spanish `START_REPLY`.
- Priority: P1 · effort M · risk 1

### D9 `config get <unset key>` doesn't name the default
- Evidence: fresh root `config get mode` → `mode is not set (default applies)` (`config_cli.rs:151-153`) while the registry row carries `default: "ask"` (`config_keys.rs:134-141`).
- Impact: the operator must run `config schema` to learn what will actually happen.
- Proposal: print `mode is not set (default: ask)` by reading `lookup(&key).default`.
- Acceptance: on a fresh root `config get mode` prints `(default: ask)`.
- Priority: P2 · effort S · risk 1

### D10 No snapshot or diff before `config set`/`config edit` overwrites the file
- Evidence: `write_doc` (`config_file.rs:118-137`) validates then renames over the target — no `.bak`, no diff (`grep -in 'backup|\.bak|diff' config_file.rs` → nothing); `ka-agent backup` includes the config only when run explicitly.
- Impact: a semantically bad value is unrecoverable except from memory or an old manual backup.
- Proposal: keep one generation `<config>.bak` on every set/unset/edit write in `write_doc`, and print the changed-key names via `config_keys::changed(old, new)`.
- Acceptance: after `config set mode allow`, `<config>.bak` holds the previous text; a second set rotates it.
- Priority: P2 · effort S · risk 1

### D11 Profiles are one-time full copies — no overlay or inheritance
- Evidence: `derive_config` (`apps/ka-agent/src/profile_cli.rs:180-194`) copies the source config once at `profile create` (minus channels/workspace, own web port); no inheritance mechanism exists.
- Impact: changing budgets/model/timezone on `default` must be repeated per profile and drifts.
- Proposal: minimum: state "a profile's config is an independent copy" in `profile create --help` and the README profiles section; optional later: `inherit = "default"` merged in `DaemonConfig::load`.
- Acceptance: `ka-agent profile create --help` says the new profile's config is a one-time copy that does not track its source.
- Priority: P2 · effort S (doc) / L (inherit) · risk 1

### D12 Two unrelated grant systems share the `grant` name
- Evidence: `ka-agent grant --help` → "Manage tool grants (what the agent may reach: hosts, repos, …)" and only touches the scope store in state.db (`grant.rs:19-54`); `[[grants.path]]`/`grants.install`/`grants.full` are config keys (`config.rs:277-296`) invisible to that command.
- Impact: the operator runs `ka-agent grant list` expecting their path grants and sees `no grants`.
- Proposal: reword `grant` help to "connector/tool scopes"; make `grant list`'s empty state hint at `[[grants.path]]` (or fold path grants in per D1).
- Acceptance: with `[[grants.path]]` configured, `ka-agent grant list` lists those rows or prints a hint naming `[[grants.path]]`.
- Priority: P2 · effort S · risk 1

### D13 Doctor diagnoses but never repairs — no `--fix`
- Evidence: `doctor --help` shows only `--online`/`--json`; the perms check prints "… — fixed on the next start, or now: chmod 700 {}" (`doctor.rs:490`), handing the operator a command instead of running it.
- Impact: the operator hand-copies chmod commands for issues the tool already diagnosed precisely.
- Proposal: `ka-agent doctor --fix` applying the safe repairs (dir modes 700, secret files 0600) and printing what changed; everything else stays advisory.
- Acceptance: with a world-readable secrets dir, `doctor --fix` sets 700 and the next `doctor` run is green on that line.
- Priority: P2 · effort S · risk 1

### E1 web-ui.md is missing 8 route groups and denies the web chat exists
- Evidence: `for r in /status /tasks /skills /contacts /chat/send /memory/forget /events /api/; do grep -c -- "$r" docs/contract/web-ui.md; done` → all 0. The routes live at `crates/ka-agent-web/src/lib.rs:487-537` (POST `/chat/send`, GET `/status`, `/tasks`, POST `/tasks/cancel`, GET `/skills`, POST `/skills/toggle`, GET `/contacts`, POST `/memory/forget`), plus `/events` (lib.rs:437,549) and bearer-token `/api/notify`/`/api/ask` (lib.rs:466-471, 645-728). web-ui.md:137-138 still says "Not covered by the UI on purpose: chat …" — false. The doc's hash is pinned by `doctor` (`doctor.rs:74-75`). Route list re-verified first-hand by the lead.
- Impact: the contract is the operator's map of the panel and doctor pins its hash — it actively misleads, denying the web chat and hiding the local scripting API.
- Proposal: rewrite the route table in `docs/contract/web-ui.md` from lib.rs:475-541 plus `/events` and `/api/*`; delete the stale "Not covered" paragraph; refresh the settings list to the 19 `web: true` keys.
- Acceptance: every route above appears in web-ui.md and `grep -c "Not covered by the UI on purpose" docs/contract/web-ui.md` is 0.
- Priority: P1 · effort S · risk 1

### E2 `/history`, `/history/show`, `/audit`, `/intents` render without the sidebar — dead-end pages
- Evidence: authenticated curls of the ten pages: `<aside>` count 1 for seven, 0 for `/history`, `/audit`, `/intents` (and `/history/show/<id>`). `crates/ka-agent-web/src/pages.rs:565,593,618` (history), `:902` (audit), `:923` (intents) build with `runtime_page(...)` instead of the `nav_page(...)` the other nine use (pages.rs:73).
- Impact: opening History or Audit lands on a page with no navigation — the only way out is the browser back button (painful on mobile).
- Proposal: switch those four builders in `pages.rs` to `nav_page(runtime, active, …)` with the matching active marker.
- Acceptance: authenticated `curl …/history`, `…/audit`, `…/intents` each contain `<aside>` (grep count 1).
- Priority: P1 · effort S · risk 1

### E3 `/intents` is reachable but linked nowhere
- Evidence: nav item table `pages.rs:79-90` lists 10 links — `/intents` absent; `curl -o /dev/null -w '%{http_code}' …/intents` → 200.
- Impact: the MCP intent log (what tools a server was asked to run) can only be found by typing the URL.
- Proposal: add `("/intents", "Intents", 0)` to the `items` array in `nav_page` (pages.rs:79).
- Acceptance: `curl -b jar …/ | grep -c 'href="/intents"'` → 1.
- Priority: P2 · effort S · risk 1

### E4 History search is literal-phrase-only — no AND, no prefix, no operators
- Evidence: seeded episodes "please deploy nginx to staging" / "check the cache hit rate": `?q=deploy` → 2 hits; `?q=deploy+cache` → 0; `?q=deploy+AND+nginx` → 0; `?q=ngin*` → 0. Cause: `crates/ka-agent-store/src/turns.rs:222-235` wraps the whole raw query in one FTS5 phrase (injection-safe, but no query language). Code re-verified first-hand by the lead.
- Impact: two keywords return silence unless the operator guesses the exact contiguous phrasing the model used — search feels broken.
- Proposal: in `search_episodes` (turns.rs:225) split on whitespace, quote each token, join with `" AND "` (still injection-safe); keep phrase search for quoted input.
- Acceptance: with those episodes, `?q=deploy+cache` returns ≥ 1 row.
- Priority: P1 · effort S · risk 1

### E5 No per-model spend split on `/usage` or `ka-agent usage` — the data is not even recorded
- Evidence: `/usage` renders `day|turns|tokens|cost` (`pages.rs:956-980`; `store.usage_rows_since`, `crates/ka-agent-store/src/turns.rs:184-200`, selects only `created_ms, cost_usd, tokens`); the `turns` table has no model column (`crates/ka-agent-store/src/schema_v1.sql:17-28`). CLI (`apps/ka-agent/src/usage_cli.rs:40`) reads the same rows — exact parity, so neither surface can split.
- Impact: an operator mixing a cheap background model with an expensive main/fallback model cannot see which one drives spend before the daily cap.
- Proposal: add `turns.model` (schema migration + turn recorder), extend `usage_rows_since` to return it, GROUP BY model in `pages::usage` and `usage_cli.rs`.
- Acceptance: after two scripted turns under different models, `/usage` and `ka-agent usage --json` show a per-model breakdown.
- Priority: P1 · effort M · risk 1

### E6 Sidebar badges (approvals / memory review) are reload-only
- Evidence: badges render server-side per request (`pages.rs:73-90`); `chat.js` handles SSE events but only touches `#log` bubbles (`crates/ka-agent-web/src/chat.js:142-306`) — observed live: approval arrives via SSE, badge stays stale until reload.
- Impact: the operator sitting on Chat sees the inline card but the persistent "N waiting" signal is stale.
- Proposal: in `chat.js`, on approval/memory-candidate events adjust the badge of the corresponding `aside a` (stable ids or data attributes from `nav_page`).
- Acceptance: with an SSE-connected page, a pending approval changes the Approvals badge text without a reload.
- Priority: P2 · effort S · risk 1

### E7 `POST /memory/forget` swallows failures — an unknown id silently "succeeds"
- Evidence: `curl -i -b jar --data-urlencode 'id=bogus123' …/memory/forget` → `303` and the page unchanged; `crates/ka-agent-web/src/pages.rs:543-547` — `let _ = runtime.forget(&id); Response::see_other("/memory")`.
- Impact: the operator clicks forget, gets redirected, believes the fact is gone — the agent still remembers it.
- Proposal: match on `runtime.forget` like `memory_candidate` does (pages.rs:754-786): failure → flash page "no memory entry matches <id>"; redirect only on success.
- Acceptance: POST `/memory/forget` with a bogus id returns a page containing an error, not a bare 303.
- Priority: P2 · effort S · risk 1

### E8 Fresh chat page has an empty log and zero guidance
- Evidence: on a brand-new root the authed index page's `#log` is empty; the only affordance is the textarea placeholder (`crates/ka-agent-web/src/pages.rs:268-299` renders nothing for zero turns; every sister page has explicit empty-state copy, e.g. pages.rs:523,355,431). Merged with C9.
- Impact: a first-time web visitor sees a blank box and must guess that typing starts a real (billed) turn.
- Proposal: in `chat_page`, when `turns` is empty render one `.bubble.system` hint ("Ka is ready — say hi; commands start with /").
- Acceptance: fresh-root GET `/` contains a non-empty element inside `<div id='log'>`.
- Priority: P2 · effort S · risk 1

### E9 chat.js ignores `final_chunk` (and `session`) events — a reconnect gap can leave the settled answer truncated
- Evidence: server emits both (observed live on the SSE stream; source `crates/ka-agent-runtime/src/runtime/events.rs:608`); the client switch (`crates/ka-agent-web/src/chat.js:142-247`) has cases for inbound/turn_started/delta/tool_*/note/notice/approval*/turn_finished/error but none for `final_chunk`/`session`. Final text comes only from accumulated deltas, lost if the client lags past the 1024-event backlog (server then injects `(missed N events)`, lib.rs:597-604).
- Impact: after a long disconnect (phone asleep through a long turn) the settled bubble can permanently show truncated text although the bus carried the full answer.
- Proposal: add a `final_chunk` case in `handleEvent` that sets `currentText = ev.text` before `finish()`.
- Acceptance: `grep -c final_chunk crates/ka-agent-web/src/chat.js` ≥ 1 (plus a reconnect e2e asserting the settled text).
- Priority: P2 · effort S · risk 1

### E10 Mobile chat height hardcodes desktop chrome — composer can sit below the fold
- Evidence: `pages.rs:35` `#chat{…height:calc(100vh - 8.4rem)}` reserves a fixed chrome budget, but the ≤760 px layout (pages.rs:23) stacks the sidebar with 10 wrapping `inline-block` links (~3 rows on a 390 px viewport), so nav + `100vh-8.4rem` overflows and pushes the send form below the fold. CSS re-verified first-hand by the lead.
- Impact: on a phone the operator may scroll past the wrapped nav on every chat screen before seeing the input.
- Proposal: under the `@media(max-width:760px)` block, size `#chat` with `min-height:0` inside a flex column layout (`.layout{height:100vh}` + `main{display:flex}` + `#chat{flex:1;height:auto}`).
- Acceptance: at a 390 px viewport the send button is fully visible without scrolling (devtools check).
- Priority: P2 · effort S · risk 1

### E11 Reminders can only be cancelled from the web — no snooze or edit
- Evidence: `/tasks` rows render exactly one mutation form (`action=/tasks/cancel` — `curl -b jar …/tasks | grep -o 'action=[a-z/]*' | sort -u`); the route table (lib.rs:500-504) has only `GET /tasks` and `POST /tasks/cancel`.
- Impact: to move a reminder 30 minutes the operator must cancel and retype `/remind <when> <text>` from scratch.
- Proposal: add `POST /tasks/snooze` (id + when) reusing runtime scheduling; render a small snooze form per row in `pages::tasks` (pages.rs:341-391). Cron editing stays CLI.
- Acceptance: a reminder row can be postponed via POST `/tasks/snooze` and the "next" column shows the new time.
- Priority: P2 · effort S · risk 1

### F1 `ask --wait` does not wait — and `ask` prints nothing at all
- Evidence: `time ka-agent ask --wait hi` against a 4 s scripted turn → `rc=0, elapsed=0.034 s`, zero output. `triggers_cli.rs:86` sends `"wait": wait` but the handler `apps/ka-agent/src/ipc/server.rs:132-146` reads only `text`/`background` and replies `{"ok":true,"queued":false}` (hardcoded) as soon as `handle_message` returns; without `--wait` the reply lands only on attached surfaces (silent success AND silent failure). Handler re-verified first-hand by the lead. Merged with C1.
- Impact: as a script/pipe user I get no answer and no feedback from `ask`, even when I explicitly ask it to wait; a rejected key looks like success.
- Proposal: implement the wait (attach and stream until `TurnFinished` — the logic exists as `run_once_turn`, `apps/ka-agent/src/chat.rs:785-833`) or return the reply in the op response; print the final answer on `--wait`, a queued-line otherwise; non-zero exit on turn failure.
- Acceptance: with a 4 s scenario, `time ka-agent ask --wait hi` takes ≥ 4 s, exits 0, prints the reply; a scripted auth failure exits non-zero with the error.
- Priority: P1 · effort M · risk 1

### F2 No Retry affordance after `rate_limit`/`network` errors; an interrupted turn never re-runs
- Evidence: `docs/contract/ka-port.md:170-171` promises "offer a **Retry** button" for `rate_limit`/`network`; the runtime special-cases only `auth` (`events.rs:679-711`); every surface renders errors as plain text (term `view.rs:371-373`, telegram `surface.rs:659-661`, matrix `surface.rs:595`); no `retry` string or `/retry` command exists anywhere (`grep -rn 'Retry' crates apps` → comment + test fixture only — re-verified first-hand by the lead). Crash test (kill -9 worker mid-turn): question marked `Interrupted` (`events.rs:712-727`), piped path even says "nothing was re-submitted". Merged with B3.
- Impact: when the provider rate-limits or the worker crashes mid-answer, I must retype the whole question from memory; there is no button, key, or command.
- Proposal: a runtime helper re-submitting the last errored/interrupted turn's `inbound_text` (store keeps it; e.g. `Runtime::resend` in `crates/ka-agent-runtime/src/runtime/intake.rs`), surfaced as `/retry` in the router (`commands/`), a Telegram inline-keyboard `retry:<turn-id>` in the Error arm (`telegram/surface.rs:659`), and a hint line on the terminal.
- Acceptance: in a scripted `fail:rate_limit` → text scenario, the piped/TUI output offers `/retry` and `/retry` re-runs the same inbound text (transcript shows it twice).
- Priority: P0 · effort M · risk 2

### F3 The fallback model hop is invisible — the port drops `ModelChanged`
- Evidence: `crates/ka-agent-port/src/port/events.rs` production match ends `_ => return None` and the test `unmapped_events_are_dropped` (:426-441) explicitly drops `Event::ModelChanged`; `AgentEvent::Settings` fires only on config reload (`crates/ka-agent-runtime/src/runtime/config.rs:265`); `StatusReport` prints the configured chain (`status.rs:111-113`) and never changes when the engine falls back. Observed live: retry ladder visible, but no line says which model actually answered. If the pinned engine (ka-harness `193ad5d`) does not emit `ModelChanged` on fallback hops, this is `needs-pin-bump`.
- Impact: with a fallback chain I cannot tell whether the primary or the fallback answered a given turn — a quality drop or dead primary goes undiagnosed.
- Proposal: map `Event::ModelChanged` in `port/events.rs` to a settings-style event; render `model X (fallback)` in the TUI status line and turn footer; add the live model to `StatusReport`.
- Acceptance: `map_ka_event(ModelChanged)` returns `Some(..)` (replacing the drop assertion at port/events.rs:426-441) and a scripted hop shows the new model in the term status line.
- Priority: P1 · effort S · risk 1 · needs-pin-bump

### F4 Update discovery is manual-only; `status` is blind to it
- Evidence: `StatusReport` (`crates/ka-agent-runtime/src/status.rs:7-41`) has no update field; `record_check` (`apps/ka-agent/src/update_cli.rs:334-346`) fires only inside the manual `update`/`update check` CLI; the sole consumer of `CHECK_META` is `doctor.rs:470-486` (7-day window). No daemon task/timer runs a check (`grep -rn 'record_check|CHECK_META'` across the tree). Live: `ka-agent status` on a running daemon prints no update line. Merged with C8.
- Impact: an operator who never types `update check` (the common case) runs a stale binary forever — doctor's "update available" can only appear after a manual check they don't know to run.
- Proposal: weekly daemon-side check (a periodic task in `apps/ka-agent/src/host.rs` reusing `update_cli::fetch_release`/`record_check`), `update_available: Option<String>` in `StatusReport` + a render line in `status.rs`, echoed in the TUI status line, with a Notice on a new release.
- Acceptance: with a `KA_AGENT_UPDATE_API` fixture advertising a newer tag, a daemon running past the tick records `update.checked` and `ka-agent status` prints an update line — no manual CLI call.
- Priority: P1 · effort M · risk 1

### F5 Piped/local `chat` lingers the full `EOF_WAIT = 120 s` after stdin EOF
- Evidence: `apps/ka-agent/src/chat.rs:21` `EOF_WAIT = 120 s`. Timestamped local run: banner +0.00 s, turn done +0.08 s, `Ka rests.` at +119.83 s (second run 120.18 s); the attached path exits in ~5 s for the same flow. The EOF handshake (`chat.rs:728-736`) only breaks on a subsequent `Hello` reporting `Idle`/queue 0 (`chat.rs:607-624`), which the local backend's ordering never delivers in this race; existing e2e tests kill the child, masking it.
- Impact: every scripted/CI use of `printf … | ka-agent chat` pays a silent 2-minute tail after the answer; the command feels hung.
- Proposal: in the EOF state also break on the first `Session{Idle}`/terminal `TurnFinished` for the local backend (or poll `request_status` on a short timer); add an e2e `piped_chat_exits_promptly_after_eof`.
- Acceptance: `time (printf 'hi\n' | ka-agent chat --config … --scenario one-step.json)` finishes < 3 s total.
- Priority: P1 · effort S · risk 1

### F6 The engine retry note "(esc cancels)" is wrong on every ka-agent surface
- Evidence: observed on piped chat during a scripted rate-limit ladder: `· ↻ retrying in 5s (esc cancels)` (engine `Note` → `PortEvent::Note`, rendered at `chat.rs:300-306`; Telegram shows notes verbatim). In the TUI, `Key::Esc` only clears completions (`term/mod.rs:173`); the interrupt is Ctrl-C. The string comes from the pinned engine — `needs-pin-bump` for the wording itself.
- Impact: the one hint shown while the operator waits through a rate-limit names a key that does nothing, on every surface.
- Proposal: bind TUI `Esc` while busy to `/stop` (`term/mod.rs` `next_action`); strip or rewrite "(esc cancels)" in the non-local surface Note arms (`telegram/surface.rs`, `matrix/surface.rs`).
- Acceptance: a pty TUI run during a scripted retry: Esc aborts the turn; the Telegram draft contains no "esc".
- Priority: P2 · effort S · risk 1 · needs-pin-bump (wording)

### F7 Offline `models` silently degrades to the pinned catalog
- Evidence: `KA_AGENT_MODELS_REGISTRY=http://127.0.0.1:1/ ka-agent models openai` prints the catalog list with no footer — the "live N from models.dev"/"catalog only" footer (`apps/ka-agent/src/models_live.rs:195-207`) renders only when a snapshot exists; `models` ignores `load() == None` by design (`model_cli.rs:113-115`). Cache `<root>/cache/models-dev.json`, TTL 24 h (`models_live.rs:29,69-94`).
- Impact: on a plane or behind a firewall I cannot tell today's registry from the shipped snapshot; a stale cache looks identical to a fresh merge.
- Proposal: when `load()` is `None`, print a `catalog only (live registry unreachable)` footer (models_live.rs + model_cli.rs:132-134); optionally mark a stale-but-present cache.
- Acceptance: with the registry pointed at an unreachable host, `ka-agent models openai` ends with a `catalog only` line.
- Priority: P2 · effort S · risk 1

### F8 `status` shows no channel health — Telegram/Matrix poll failures are log-only
- Evidence: `StatusReport` (`status.rs:7-41`) has no channel fields; Telegram poll errors go to stderr only (`crates/ka-agent-channels/src/telegram/pairing.rs:209`); with a dead token the daemon's status keeps reading `● idle`.
- Impact: my phone goes quiet and `status` says everything is fine — I have to know to read logs to find the broken channel.
- Proposal: track last poll success/error per channel in `Shared` (e.g. `channel_health: HashMap<String, (ok_since_ms, last_error)>`), expose it in `StatusReport`, render `telegram: erroring since HH:MM (…)` in `status.rs::render`; doctor reuses it.
- Acceptance: with the poll pointed at an unreachable API, `ka-agent status --json` includes a channel error field and the text render shows it.
- Priority: P2 · effort M · risk 1

### F9 `models --probe` without a key doesn't say which key
- Evidence: `ka-agent models --probe openai/gpt-5.2` (no key) → `· openai/gpt-5.2 not probed (no key)` (`ProbeOutcome::NotProbed`, `models_live.rs:311`), exit 0 — unlike preflight's `"{model} needs {name} — ka-agent key set {name}"` (`intake.rs:407-410`) and `model`'s `✗ OPENAI_API_KEY missing — ka-agent key set OPENAI_API_KEY` (`model_cli.rs:36-38`).
- Impact: after probing I still don't know which env var to set, unlike every other key-related message in the product.
- Proposal: carry the `key_env` name into `ProbeOutcome::NotProbed` (resolve it where the selector resolves, `model_cli.rs:170-183`) and print `not probed (no key) — ka-agent key set OPENAI_API_KEY`.
- Acceptance: the probe line names the env var for a keyed provider.
- Priority: P2 · effort S · risk 1

### F10 The crash notice doesn't name the lost prompt; restart backoff is silent
- Evidence: kill -9 of the worker child mid-turn: chat got `· engine closed; the next message restarts the worker` (`worker.rs:248-255` wording), turn `Interrupted` (`events.rs:712-727`); the prompt text appears nowhere. Restart backoff (1 s → 60 s, `worker.rs:6-37`, slept at `:48-55`) gives no "respawning in Ns" note, so a message sent right after a crash-loop can appear frozen.
- Impact: after a crash I don't know which question died, and my next message can look stuck while the daemon backs off.
- Proposal: include the lost prompt's first line in the engine-closed Notice (`worker.rs:248-255`); emit a `respawning worker in Ns (backoff)` Note when the delay is non-trivial. (Re-running the text is F2's `/retry`.)
- Acceptance: kill -9 mid-turn → the Notice names the interrupted prompt; a second crash within the window shows the backoff note on attached surfaces.
- Priority: P2 · effort S · risk 2

## Rejected

- Telegram reply-to threading (`reply_parameters` on final answers) — cosmetic in a 1:1 single-operator chat; queued answers arrive in order, and nothing in the crate carries `message_id` today (grep verified). Not worth the plumbing.
- "Queue depth missing from the TUI status line" (plan seed) — false: `apps/ka-agent/src/term/view.rs:89-91` renders `queued N`, fed by the `Queue` arm (`view.rs:355`).
- "No transcript search / no way to view a previous result" (seed) — false: `/history [query]` and `/history show <id>` exist on every surface (`commands/memory.rs:152-167`); their TUI invisibility is A1, not a missing feature.
- "Telegram drops notices while paused" — false: `Notice` (operator-must-see) is always delivered as its own message (`telegram/surface.rs:494-498`); only in-turn `Note`s are gated. Pause/degraded/budget/auth remedies all reach the phone.
- "Draft streaming / 429 policy gaps" — implemented and triple-tested (`telegram_e2e.rs:650-805`: flood control keeps drafts streaming; 400/404 falls back; throttle ignores growth past the cap).
- "Missed reminders invisible after daemon downtime" — handled per `cron-and-refs.md`: `(late by …)` within the grace hour, `missed: … (overdue by …)` beyond (`scheduler.rs:129-186`), delivered as own messages.
- "SSE gap on phone sleep untested / no resume" — protocol works: `id:` per event, `Last-Event-ID`/`?since=` ring replay verified live (ring 256, native EventSource reconnects); the residual client-side gap is E9, not the transport.
- "Env overrides undocumented" (seed) — false: all operator-facing `KA_AGENT_*` are documented (`KA_AGENT_HOME/DATA_DIR/CONFIG/PROFILE` README:176-202, installer knobs README:25-26, `KA_AGENT_SECRET_*` telegram.md:125/matrix.md:48/plugins.md:71, `KA_AGENT_GUARD_GRANTS` grants.md:119); the remainder (`KA_AGENT_UPDATE_API`, `KA_AGENT_MODELS_REGISTRY`, …) are test seams noted in code comments.
- "Demo mode without keys" (seed) — rejected by design: local providers (ollama/lmstudio/llamacpp/vllm) are the no-key path and the wizard auto-prefers a running one (`setup/provider.rs:20-65`, probed "(running)").
- Reaction-based approvals on Telegram — the inline keyboard is strictly better than Matrix's ✅/❌ (one tap, exactly-once answers, settles the card); 👀 already covers the accept receipt (`channel.rs:431-436`, tested).
- Read receipts beyond 👀 — platform limit: bots get no read status; no ka-agent-side work possible.
- "Registry has unregistered leaves" (seed) — false: the 44 `KeySpec` rows map 1:1 to `DaemonConfig` leaves both ways (unit test `every_leaf_of_default_and_full_fixture_is_registered`, `config_keys.rs:551`).
- "Config comments/order lost on `config set`" — false: reproduced on the commented example file, diff clean except the value (toml_edit decor, `config_file.rs:53-72`).
- Setup honesty / abort / resume regressions — none: fresh-root no-arg and `chat` messages are honest (v1 A1 holds), abort leaves nothing on disk, re-run resumes via the section menu, mid-write failure says what landed (setup_e2e.rs:467-513).
- Cron loops / cron preflight (v1 G4/G5) — shipped and covered by existing tests; no regression found.
- "Always allow" absent from the phone — deliberate (gap G12, `ka-port.md:62-66`); session-allow stays console/web and `/new` clears it.
- Per-chunk "(cont.)" markers on long answers — in-order chunks make them noise; fence-closing and ≥1 s spacing already tested.
- Cold-start performance — fine: scripted local chat first answer +0.08 s, `status` 6 ms, `--version` 3 ms (debug build, this machine); nothing to fix.
- Windows-specific findings — out of scope on this Linux host; `docs/contract/windows.md` already lists its own host-unverified items.
